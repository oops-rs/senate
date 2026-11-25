//! A scene bundle folder, checked: plain files of allowed types within the
//! size limits, no links (symbolic or hard), no path that could leave the
//! folder, and a valid manifest whose entry is among them.

use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};

use crate::{
    MAX_DEPTH, MAX_FILE, MAX_FILES, MAX_MANIFEST, MAX_TOTAL, Manifest, Problem, content_type,
    ignored, shown, valid_relative, valid_segment,
};

/// One file of a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    /// Bundle-relative.
    pub path: String,
    pub size: u64,
}

/// A checked bundle: everything that may ever be served from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub root: PathBuf,
    pub manifest: Manifest,
    /// Sorted by path.
    pub files: Vec<BundleFile>,
    /// SHA-256 over every path and its bytes, lowercase hex: exactly what
    /// this bundle is.
    pub digest: String,
}

impl Bundle {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Check the folder at `root` and everything in it.
    pub fn scan(root: &Path) -> Result<Self, Problem> {
        let meta = fs::symlink_metadata(root).map_err(|_| Problem::NotAFolder)?;
        if !meta.file_type().is_dir() {
            return Err(Problem::NotAFolder);
        }
        let mut files = Vec::new();
        let mut total = 0;
        walk(root, "", 0, &mut files, &mut total)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let manifest_file = files
            .iter()
            .find(|f| f.path == "manifest.json")
            .ok_or(Problem::NoManifest)?;
        if manifest_file.size > MAX_MANIFEST as u64 {
            return Err(Problem::ManifestTooLarge);
        }
        let manifest_bytes =
            read(root, "manifest.json").map_err(|_| Problem::Special("manifest.json".into()))?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        if !files.iter().any(|f| f.path == manifest.entry) {
            return Err(Problem::NoEntry);
        }
        let mut hash = Sha256::new();
        for file in &files {
            let bytes = read(root, &file.path).map_err(|_| Problem::Special(shown(&file.path)))?;
            digest_file(&mut hash, &file.path, &bytes);
        }
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            files,
            digest: hex::encode(hash.finalize()),
        })
    }
}

/// One file into a bundle digest: its path, a zero byte, its length (u64
/// little-endian), its bytes. Files go in path order.
pub(crate) fn digest_file(hash: &mut Sha256, path: &str, bytes: &[u8]) {
    hash.update(path.as_bytes());
    hash.update([0]);
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: usize,
    found: &mut Vec<BundleFile>,
    total: &mut u64,
) -> Result<(), Problem> {
    let here = if prefix.is_empty() {
        ".".to_owned()
    } else {
        shown(prefix)
    };
    if depth >= MAX_DEPTH {
        return Err(Problem::BadName(here));
    }
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|_| Problem::Special(here.clone()))? {
        let entry = entry.map_err(|_| Problem::Special(here.clone()))?;
        // A name that is not UTF-8 is never a valid segment.
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    for name in names {
        if ignored(&name, depth == 0) {
            continue;
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if !valid_segment(&name) {
            return Err(Problem::BadName(shown(&path)));
        }
        let url = dir.join(&name);
        let meta = fs::symlink_metadata(&url).map_err(|_| Problem::Special(shown(&path)))?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            return Err(Problem::Link(shown(&path)));
        } else if kind.is_dir() {
            walk(&url, &path, depth + 1, found, total)?;
        } else if kind.is_file() {
            if meta.nlink() != 1 {
                return Err(Problem::Link(shown(&path)));
            }
            if content_type(&path).is_none() {
                return Err(Problem::Type(shown(&path)));
            }
            if meta.len() > MAX_FILE {
                return Err(Problem::FileTooLarge(shown(&path)));
            }
            *total += meta.len();
            if *total > MAX_TOTAL {
                return Err(Problem::TooLarge);
            }
            found.push(BundleFile {
                path,
                size: meta.len(),
            });
            if found.len() > MAX_FILES {
                return Err(Problem::TooMany);
            }
        } else {
            // FIFOs, sockets, devices: never part of a scene.
            return Err(Problem::Special(shown(&path)));
        }
    }
    Ok(())
}

/// Why one file could not be read from a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("not a bundle path")]
    Path,
    #[error("not found")]
    NotFound,
    #[error("a link")]
    Link,
    #[error("not a plain file")]
    NotPlain,
    #[error("too large")]
    TooLarge,
}

/// One file's bytes, opened from `root` one component at a time without
/// following a link at any level, checked to be a plain unshared file
/// within the size limit. (Opening is non-blocking, so a FIFO planted after
/// a scan cannot stall the caller.)
pub fn read(root: &Path, path: &str) -> Result<Vec<u8>, ReadError> {
    if !valid_relative(path) {
        return Err(ReadError::Path);
    }
    let base = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut fd = rustix::fs::open(root, base | OFlags::DIRECTORY, Mode::empty())
        .map_err(|_| ReadError::NotFound)?;
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let flags = if last { base } else { base | OFlags::DIRECTORY };
        match rustix::fs::openat(&fd, *part, flags, Mode::empty()) {
            Ok(next) => fd = next,
            Err(_) => {
                let link = rustix::fs::statat(&fd, *part, AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
                return Err(if link {
                    ReadError::Link
                } else {
                    ReadError::NotFound
                });
            }
        }
    }
    let st = rustix::fs::fstat(&fd).map_err(|_| ReadError::NotFound)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
        return Err(ReadError::NotPlain);
    }
    if st.st_nlink != 1 {
        return Err(ReadError::Link);
    }
    if u64::try_from(st.st_size).unwrap_or(u64::MAX) > MAX_FILE {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::new();
    fs::File::from(fd)
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ReadError::NotFound)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(ReadError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const MANIFEST: &str = r#"{"name":"Fixture","version":"1.0.0","entry":"index.html","observe":{"major":1,"required":[],"optional":[]},"cards":{"reads":[],"writes":[]},"min_host_version":1}"#;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("manifest.json"), MANIFEST).expect("write");
        fs::write(dir.path().join("index.html"), "<!doctype html><p>hi</p>").expect("write");
        fs::create_dir(dir.path().join("js")).expect("mkdir");
        fs::write(dir.path().join("js/app.js"), "1").expect("write");
        dir
    }

    fn problem(root: &Path) -> Option<Problem> {
        Bundle::scan(root).err()
    }

    #[test]
    fn scans_with_a_stable_digest() {
        let a = fixture();
        let b = fixture();
        let x = Bundle::scan(a.path()).expect("scan");
        assert_eq!(x.digest, Bundle::scan(b.path()).expect("scan").digest);
        assert_eq!(
            x.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["index.html", "js/app.js", "manifest.json"]
        );
        fs::write(a.path().join("js/app.js"), "2").expect("write");
        assert_ne!(x.digest, Bundle::scan(a.path()).expect("scan").digest);
    }

    #[test]
    fn links_and_special_files_are_refused() {
        let d = fixture();
        symlink("/etc/hosts", d.path().join("x.js")).expect("link");
        assert_eq!(problem(d.path()), Some(Problem::Link("x.js".into())));

        let d = fixture();
        symlink("/etc", d.path().join("assets")).expect("link");
        assert_eq!(problem(d.path()), Some(Problem::Link("assets".into())));

        let d = fixture();
        let outside = tempfile::tempdir().expect("tempdir");
        fs::write(outside.path().join("s.json"), "{}").expect("write");
        fs::hard_link(outside.path().join("s.json"), d.path().join("s.json")).expect("hard link");
        assert_eq!(problem(d.path()), Some(Problem::Link("s.json".into())));

        let d = fixture();
        let made = std::process::Command::new("/usr/bin/mkfifo")
            .arg(d.path().join("pipe.js"))
            .status()
            .expect("mkfifo");
        assert!(made.success());
        assert_eq!(problem(d.path()), Some(Problem::Special("pipe.js".into())));

        let holder = tempfile::tempdir().expect("tempdir");
        let d = fixture();
        symlink(d.path(), holder.path().join("scene")).expect("link");
        assert_eq!(
            problem(&holder.path().join("scene")),
            Some(Problem::NotAFolder)
        );
    }

    #[test]
    fn names_types_and_sizes() {
        for (name, expected) in [
            ("tool.exe", Problem::Type("tool.exe".into())),
            ("art.svg", Problem::Type("art.svg".into())),
            ("README", Problem::Type("README".into())),
            (".env.json", Problem::BadName(".env.json".into())),
            ("a b.js", Problem::BadName("a b.js".into())),
        ] {
            let d = fixture();
            fs::write(d.path().join(name), "").expect("write");
            assert_eq!(problem(d.path()), Some(expected), "{name}");
        }
        let d = fixture();
        fs::write(d.path().join("LICENSE"), "MIT").expect("write");
        fs::write(d.path().join(".DS_Store"), "").expect("write");
        fs::create_dir(d.path().join("__MACOSX")).expect("mkdir");
        assert!(Bundle::scan(d.path()).is_ok());

        let d = fixture();
        fs::File::create(d.path().join("big.png"))
            .expect("create")
            .set_len(MAX_FILE + 1)
            .expect("len");
        assert_eq!(
            problem(d.path()),
            Some(Problem::FileTooLarge("big.png".into()))
        );

        let d = fixture();
        for i in 0..9 {
            fs::File::create(d.path().join(format!("p{i}.png")))
                .expect("create")
                .set_len(MAX_FILE - 1)
                .expect("len");
        }
        assert_eq!(problem(d.path()), Some(Problem::TooLarge));

        let d = fixture();
        fs::remove_file(d.path().join("manifest.json")).expect("rm");
        assert_eq!(problem(d.path()), Some(Problem::NoManifest));
        let d = fixture();
        fs::remove_file(d.path().join("index.html")).expect("rm");
        assert_eq!(problem(d.path()), Some(Problem::NoEntry));
    }

    #[test]
    fn reading_never_leaves_the_bundle() {
        let holder = tempfile::tempdir().expect("tempdir");
        fs::write(holder.path().join("secret.txt"), "secret").expect("write");
        let root = holder.path().join("scene");
        fs::create_dir(&root).expect("mkdir");
        fs::write(root.join("index.html"), "x").expect("write");
        fs::create_dir(root.join("sub")).expect("mkdir");
        symlink("../../secret.txt", root.join("sub/up.txt")).expect("link");
        symlink(holder.path(), root.join("via")).expect("link");
        for path in [
            "../secret.txt",
            "/etc/hosts",
            "sub/../../secret.txt",
            "sub//x.txt",
            "",
            "./index.html",
        ] {
            assert_eq!(read(&root, path), Err(ReadError::Path), "{path}");
        }
        assert_eq!(read(&root, "sub/up.txt"), Err(ReadError::Link));
        assert_eq!(read(&root, "via/secret.txt"), Err(ReadError::Link));
        assert_eq!(read(&root, "missing.txt"), Err(ReadError::NotFound));
        assert_eq!(read(&root, "sub"), Err(ReadError::NotPlain));
        assert_eq!(read(&root, "index.html").expect("read"), b"x");
    }
}

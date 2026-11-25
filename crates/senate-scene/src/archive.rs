//! `.senatescene`: a scene bundle as a zip of **stored** (uncompressed)
//! entries, as `senate scene pack` makes it. Being uncompressed, every
//! entry's size is what the archive's own length bounds, so no archive can
//! unpack into more than it is. The central directory is read and checked
//! first (count, names, types, sizes, no links, no encryption, no zip64, no
//! compression); then each file is taken from its local header, which must
//! agree with the directory, and its CRC-32 checked, into an empty folder.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::bundle::digest_file;
use crate::{
    ArchiveProblem, Bundle, MAX_FILE, MAX_TOTAL, Problem, content_type, ignored, read, shown,
    valid_relative,
};

/// The largest archive.
pub const MAX_ARCHIVE: u64 = MAX_TOTAL + 1024 * 1024;
/// The most entries an archive may list (folders and sidecars included).
pub const MAX_ENTRIES: usize = 4000;

/// One file of an archive, from its central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    /// As listed, without a trailing slash.
    pub name: String,
    pub size: u64,
    pub directory: bool,
    /// A Finder sidecar (`__MACOSX/…`, `.DS_Store`): skipped.
    pub sidecar: bool,
    crc: u32,
    local: usize,
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn zip(p: ArchiveProblem) -> Problem {
    Problem::Archive(p)
}

/// The archive's entries, checked from its central directory alone.
pub fn entries(bytes: &[u8]) -> Result<Vec<ArchiveEntry>, Problem> {
    let not_zip = || zip(ArchiveProblem::NotZip);
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err(zip(ArchiveProblem::Size));
    }
    if bytes.len() < 22 {
        return Err(not_zip());
    }
    // The end of central directory record, searched for from the end.
    let lowest = bytes.len().saturating_sub(22 + 65535);
    let eocd = (lowest..=bytes.len() - 22)
        .rev()
        .find(|&i| u32_at(bytes, i) == Some(0x0605_4B50))
        .ok_or_else(not_zip)?;
    let count = usize::from(u16_at(bytes, eocd + 10).ok_or_else(not_zip)?);
    let cd_size = u32_at(bytes, eocd + 12).ok_or_else(not_zip)?;
    let cd_offset = u32_at(bytes, eocd + 16).ok_or_else(not_zip)?;
    if count == 0xFFFF || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(zip(ArchiveProblem::Zip64));
    }
    if count > MAX_ENTRIES {
        return Err(Problem::TooMany);
    }
    let (cd_offset, cd_size) = (cd_offset as usize, cd_size as usize);
    if cd_offset.checked_add(cd_size).is_none_or(|end| end > eocd) {
        return Err(not_zip());
    }
    let mut at = cd_offset;
    let mut out: Vec<ArchiveEntry> = Vec::with_capacity(count);
    let mut total: u64 = 0;
    for _ in 0..count {
        if at + 46 > eocd || u32_at(bytes, at) != Some(0x0201_4B50) {
            return Err(not_zip());
        }
        let field16 = |o| u16_at(bytes, at + o).ok_or_else(not_zip);
        let field32 = |o| u32_at(bytes, at + o).ok_or_else(not_zip);
        let made_by = field16(4)? >> 8;
        let flags = field16(8)?;
        let method = field16(10)?;
        let crc = field32(16)?;
        let compressed = field32(20)?;
        let size = field32(24)?;
        let name_len = usize::from(field16(28)?);
        let extra = usize::from(field16(30)?);
        let comment = usize::from(field16(32)?);
        let external = field32(38)?;
        let local = field32(42)?;
        let raw = bytes
            .get(at + 46..at + 46 + name_len)
            .filter(|_| at + 46 + name_len <= eocd)
            .ok_or_else(not_zip)?;
        at += 46 + name_len + extra + comment;
        let name = String::from_utf8_lossy(raw).into_owned();
        if flags & 1 != 0 {
            return Err(zip(ArchiveProblem::Encrypted));
        }
        if compressed == u32::MAX || size == u32::MAX || local == u32::MAX {
            return Err(zip(ArchiveProblem::Zip64));
        }
        // Unix mode in the high half of the external attributes.
        if made_by == 3 && (external >> 16) & 0xF000 == 0xA000 {
            return Err(Problem::Link(shown(&name)));
        }
        let directory = name.ends_with('/');
        let path = name.strip_suffix('/').unwrap_or(&name).to_owned();
        let segments: Vec<&str> = path.split('/').collect();
        let sidecar =
            segments.first() == Some(&"__MACOSX") || segments.last() == Some(&".DS_Store");
        let escapes = name.starts_with('/')
            || name.contains('\\')
            || segments.iter().any(|s| *s == ".." || *s == ".");
        if escapes || (!sidecar && (path.is_empty() || !valid_relative(&path))) {
            return Err(Problem::BadName(shown(&name)));
        }
        if !directory && !sidecar {
            if content_type(&path).is_none() {
                return Err(Problem::Type(shown(&path)));
            }
            if u64::from(size) > MAX_FILE {
                return Err(Problem::FileTooLarge(shown(&path)));
            }
            if method != 0 || compressed != size {
                return Err(zip(ArchiveProblem::Compressed));
            }
        }
        total += u64::from(size);
        if total > MAX_TOTAL {
            return Err(Problem::TooLarge);
        }
        out.push(ArchiveEntry {
            name: path,
            size: u64::from(size),
            directory,
            sidecar,
            crc,
            local: local as usize,
        });
    }
    Ok(out)
}

/// Unpack `archive` into `staging` (a new folder, created here; it must not
/// exist) and return the bundle's folder within it: `staging` itself, or
/// its one folder when everything was zipped inside one. The caller scans
/// the result ([`Bundle::scan`]) like any folder.
pub fn unpack(archive: &Path, staging: &Path) -> Result<PathBuf, Problem> {
    let io = |what: &Path| Problem::Io(shown(&what.to_string_lossy()));
    let meta = fs::symlink_metadata(archive).map_err(|_| io(archive))?;
    if !meta.file_type().is_file() {
        return Err(Problem::NotAFolder);
    }
    if meta.len() > MAX_ARCHIVE {
        return Err(zip(ArchiveProblem::Size));
    }
    let bytes = fs::read(archive).map_err(|_| io(archive))?;
    let list = entries(&bytes)?;
    let mut dir = fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
    dir.create(staging).map_err(|_| io(staging))?;
    for entry in list.iter().filter(|e| !e.directory && !e.sidecar) {
        let data = local_data(&bytes, entry)?;
        let target = staging.join(&entry.name);
        if let Some(parent) = target.parent() {
            dir.recursive(true)
                .create(parent)
                .map_err(|_| zip(ArchiveProblem::Corrupt))?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|_| zip(ArchiveProblem::Corrupt))?;
        file.write_all(data).map_err(|_| io(&target))?;
    }
    let names: Vec<String> = fs::read_dir(staging)
        .map_err(|_| io(staging))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| !ignored(n, true))
        .collect();
    if let [only] = names.as_slice()
        && only != "manifest.json"
    {
        let inner = staging.join(only);
        if fs::symlink_metadata(&inner).is_ok_and(|m| m.file_type().is_dir()) {
            return Ok(inner);
        }
    }
    Ok(staging.to_path_buf())
}

/// An entry's bytes from its local header, which must agree with the
/// central directory (signature, name, stored, sizes), with its CRC-32.
fn local_data<'a>(bytes: &'a [u8], entry: &ArchiveEntry) -> Result<&'a [u8], Problem> {
    let corrupt = || zip(ArchiveProblem::Corrupt);
    let at = entry.local;
    if u32_at(bytes, at) != Some(0x0403_4B50) {
        return Err(corrupt());
    }
    let flags = u16_at(bytes, at + 6).ok_or_else(corrupt)?;
    let method = u16_at(bytes, at + 8).ok_or_else(corrupt)?;
    let name_len = usize::from(u16_at(bytes, at + 26).ok_or_else(corrupt)?);
    let extra = usize::from(u16_at(bytes, at + 28).ok_or_else(corrupt)?);
    if flags & 1 != 0 || method != 0 {
        return Err(corrupt());
    }
    let name = bytes.get(at + 30..at + 30 + name_len).ok_or_else(corrupt)?;
    if name != entry.name.as_bytes() {
        return Err(corrupt());
    }
    let start = at + 30 + name_len + extra;
    let size = usize::try_from(entry.size).map_err(|_| corrupt())?;
    let data = bytes.get(start..start + size).ok_or_else(corrupt)?;
    if crc32(data) != entry.crc {
        return Err(corrupt());
    }
    Ok(data)
}

/// CRC-32 (IEEE), as zip uses.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// Pack the bundle at `root` into a new `.senatescene` at `out` (which must
/// not exist): the bundle is checked first, then exactly its files, sorted,
/// stored, with fixed times and modes, so the same bundle always makes the
/// same bytes. Returns the checked bundle.
pub fn pack(root: &Path, out: &Path) -> Result<Bundle, Problem> {
    let bundle = Bundle::scan(root)?;
    let mut archive: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    let mut hash = Sha256::new();
    for file in &bundle.files {
        let data = read(root, &file.path).map_err(|_| Problem::Special(shown(&file.path)))?;
        digest_file(&mut hash, &file.path, &data);
        let name = file.path.as_bytes();
        let (size, offset) = (
            u32::try_from(data.len()).map_err(|_| Problem::TooLarge)?,
            u32::try_from(archive.len()).map_err(|_| Problem::TooLarge)?,
        );
        let name_len =
            u16::try_from(name.len()).map_err(|_| Problem::BadName(shown(&file.path)))?;
        let crc = crc32(&data);
        // Local header: version 2.0, no flags, stored, 1980-01-01 00:00.
        archive.extend(0x0403_4B50u32.to_le_bytes());
        for v in [20u16, 0, 0, 0, 0x0021] {
            archive.extend(v.to_le_bytes());
        }
        for v in [crc, size, size] {
            archive.extend(v.to_le_bytes());
        }
        for v in [name_len, 0] {
            archive.extend(v.to_le_bytes());
        }
        archive.extend(name);
        archive.extend(&data);
        // Central header: made by Unix 2.0, a plain 0644 file.
        central.extend(0x0201_4B50u32.to_le_bytes());
        for v in [0x0314u16, 20, 0, 0, 0, 0x0021] {
            central.extend(v.to_le_bytes());
        }
        for v in [crc, size, size] {
            central.extend(v.to_le_bytes());
        }
        for v in [name_len, 0, 0, 0, 0] {
            central.extend(v.to_le_bytes());
        }
        for v in [0o100_644u32 << 16, offset] {
            central.extend(v.to_le_bytes());
        }
        central.extend(name);
    }
    if hex::encode(hash.finalize()) != bundle.digest {
        // A file changed while it was being packed.
        return Err(Problem::Io(shown(&root.to_string_lossy())));
    }
    let count = u16::try_from(bundle.files.len()).map_err(|_| Problem::TooMany)?;
    let start = u32::try_from(archive.len()).map_err(|_| Problem::TooLarge)?;
    let central_len = u32::try_from(central.len()).map_err(|_| Problem::TooLarge)?;
    archive.extend(&central);
    archive.extend(0x0605_4B50u32.to_le_bytes());
    for v in [0u16, 0, count, count] {
        archive.extend(v.to_le_bytes());
    }
    for v in [central_len, start] {
        archive.extend(v.to_le_bytes());
    }
    archive.extend(0u16.to_le_bytes());
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(|_| Problem::Io(shown(&out.to_string_lossy())))?;
    file.write_all(&archive)
        .map_err(|_| Problem::Io(shown(&out.to_string_lossy())))?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"name":"Fixture","version":"1.0.0","entry":"index.html","observe":{"major":1,"required":[],"optional":[]},"cards":{"reads":[],"writes":[]},"min_host_version":1}"#;

    /// A hand-made zip, so tests can write what no tool would.
    /// Name, bytes, mode, flags, method, claimed size.
    type Entry = (String, Vec<u8>, u32, u16, u16, Option<u32>);

    #[derive(Default)]
    struct Zip(Vec<Entry>);

    impl Zip {
        fn add(&mut self, name: &str, data: &str) -> &mut Self {
            self.0
                .push((name.into(), data.as_bytes().to_vec(), 0o100_644, 0, 0, None));
            self
        }
        fn with(&mut self, mode: u32, flags: u16, method: u16, claimed: Option<u32>) -> &mut Self {
            if let Some(last) = self.0.last_mut() {
                (last.2, last.3, last.4, last.5) = (mode, flags, method, claimed);
            }
            self
        }
        fn build(&self) -> Vec<u8> {
            let (mut out, mut cd) = (Vec::new(), Vec::new());
            for (name, data, mode, flags, method, claimed) in &self.0 {
                let off = out.len() as u32;
                let crc = crc32(data);
                let size = data.len() as u32;
                out.extend(0x0403_4B50u32.to_le_bytes());
                for v in [20u16, *flags, *method, 0, 0] {
                    out.extend(v.to_le_bytes());
                }
                for v in [crc, size, size] {
                    out.extend(v.to_le_bytes());
                }
                for v in [name.len() as u16, 0] {
                    out.extend(v.to_le_bytes());
                }
                out.extend(name.as_bytes());
                out.extend(data);
                cd.extend(0x0201_4B50u32.to_le_bytes());
                for v in [0x031Eu16, 20, *flags, *method, 0, 0] {
                    cd.extend(v.to_le_bytes());
                }
                for v in [crc, claimed.unwrap_or(size), claimed.unwrap_or(size)] {
                    cd.extend(v.to_le_bytes());
                }
                for v in [name.len() as u16, 0, 0, 0, 0] {
                    cd.extend(v.to_le_bytes());
                }
                for v in [mode << 16, off] {
                    cd.extend(v.to_le_bytes());
                }
                cd.extend(name.as_bytes());
            }
            let start = out.len() as u32;
            out.extend(&cd);
            out.extend(0x0605_4B50u32.to_le_bytes());
            for v in [0u16, 0, self.0.len() as u16, self.0.len() as u16] {
                out.extend(v.to_le_bytes());
            }
            for v in [cd.len() as u32, start] {
                out.extend(v.to_le_bytes());
            }
            out.extend(0u16.to_le_bytes());
            out
        }
    }

    fn refusal(z: &Zip) -> Option<Problem> {
        entries(&z.build()).err()
    }

    #[test]
    fn the_directory_is_checked_first() {
        assert_eq!(
            refusal(
                Zip::default()
                    .add("index.html", "/etc/hosts")
                    .with(0o120_777, 0, 0, None)
            ),
            Some(Problem::Link("index.html".into()))
        );
        for name in [
            "../evil.js",
            "/abs.js",
            "a\\b.js",
            "a/../../b.js",
            "__MACOSX/../x.js",
        ] {
            assert_eq!(
                refusal(Zip::default().add(name, "x")),
                Some(Problem::BadName(name.into())),
                "{name}"
            );
        }
        assert_eq!(
            refusal(Zip::default().add("run.sh", "x")),
            Some(Problem::Type("run.sh".into()))
        );
        assert_eq!(
            refusal(Zip::default().add("a.png", "x").with(
                0o100_644,
                0,
                0,
                Some(MAX_FILE as u32 + 1)
            )),
            Some(Problem::FileTooLarge("a.png".into()))
        );
        let mut many = Zip::default();
        for i in 0..9 {
            many.add(&format!("p{i}.png"), "x")
                .with(0o100_644, 0, 0, Some(MAX_FILE as u32 - 1));
        }
        assert_eq!(refusal(&many), Some(Problem::TooLarge));
        assert_eq!(
            refusal(Zip::default().add("a.js", "x").with(0o100_644, 1, 0, None)),
            Some(zip(ArchiveProblem::Encrypted))
        );
        assert_eq!(
            refusal(Zip::default().add("a.js", "x").with(0o100_644, 0, 8, None)),
            Some(zip(ArchiveProblem::Compressed))
        );
        assert_eq!(
            entries(b"not a zip at all, not at all").err(),
            Some(zip(ArchiveProblem::NotZip))
        );
    }

    #[test]
    fn pack_is_deterministic_and_unpacks_to_the_same_bundle() {
        let src = tempfile::tempdir().expect("tempdir");
        fs::write(src.path().join("manifest.json"), MANIFEST).expect("write");
        fs::write(src.path().join("index.html"), "<p>hi</p>").expect("write");
        fs::create_dir(src.path().join("js")).expect("mkdir");
        fs::write(src.path().join("js/app.js"), "1").expect("write");
        fs::write(src.path().join(".DS_Store"), "junk").expect("write");
        let out = tempfile::tempdir().expect("tempdir");
        let a = pack(src.path(), &out.path().join("a.senatescene")).expect("pack");
        let b = pack(src.path(), &out.path().join("b.senatescene")).expect("pack");
        assert_eq!(a.digest, b.digest);
        let bytes = fs::read(out.path().join("a.senatescene")).expect("read");
        assert_eq!(
            bytes,
            fs::read(out.path().join("b.senatescene")).expect("read")
        );
        assert!(
            pack(src.path(), &out.path().join("a.senatescene")).is_err(),
            "never overwrites"
        );
        let root = unpack(
            &out.path().join("a.senatescene"),
            &out.path().join("staging"),
        )
        .expect("unpack");
        assert_eq!(Bundle::scan(&root).expect("scan").digest, a.digest);
    }

    #[test]
    fn unpack_takes_one_top_folder_and_checks_every_file() {
        let out = tempfile::tempdir().expect("tempdir");
        let mut z = Zip::default();
        z.add("Fixture/manifest.json", MANIFEST)
            .add("Fixture/index.html", "<p>hi</p>")
            .add("__MACOSX/Fixture/._index.html", "x");
        fs::write(out.path().join("f.senatescene"), z.build()).expect("write");
        let root =
            unpack(&out.path().join("f.senatescene"), &out.path().join("s")).expect("unpack");
        assert!(root.ends_with("Fixture"));
        assert!(!out.path().join("s/__MACOSX").exists());
        assert!(Bundle::scan(&root).is_ok());

        // A file whose bytes don't match its CRC.
        let mut bytes = Zip::default()
            .add("manifest.json", MANIFEST)
            .add("index.html", "<p>hi</p>")
            .build();
        let at = bytes
            .windows(9)
            .position(|w| w == b"<p>hi</p>")
            .expect("data");
        bytes[at] = b'X';
        fs::write(out.path().join("bad.senatescene"), bytes).expect("write");
        assert_eq!(
            unpack(&out.path().join("bad.senatescene"), &out.path().join("t")).err(),
            Some(zip(ArchiveProblem::Corrupt))
        );

        // The same name twice.
        let twice = Zip::default()
            .add("index.html", "a")
            .add("index.html", "b")
            .build();
        fs::write(out.path().join("twice.senatescene"), twice).expect("write");
        assert_eq!(
            unpack(&out.path().join("twice.senatescene"), &out.path().join("u")).err(),
            Some(zip(ArchiveProblem::Corrupt))
        );
    }
}

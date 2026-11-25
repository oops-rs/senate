//! The rules a Senate scene bundle must satisfy (docs/observe.md, "Scene
//! host contract"; docs/scenes.md, "Bundle rules"), in one place: the
//! macOS app checks and serves installed scenes with them (through
//! `senate-app-core`), and `senate scene inspect` / `senate scene pack`
//! apply exactly the same ones for a scene's author.
//!
//! A scene is untrusted code from somewhere else. Everything here reads it
//! defensively: the manifest strictly (no duplicate keys, no unknown
//! fields), the files without following any link, the archive's directory
//! before a byte of it is unpacked.

mod archive;
mod bundle;
mod manifest;

pub use archive::{ArchiveEntry, entries, pack, unpack};
pub use bundle::{Bundle, BundleFile, ReadError, read};
pub use manifest::Manifest;

/// The scene host contract version this implementation speaks.
pub const HOST_VERSION: u32 = 1;
/// The Observe major a scene may ask for.
pub const OBSERVE_MAJOR: u32 = 1;
/// The largest manifest.
pub const MAX_MANIFEST: usize = 16 * 1024;
/// The largest file in a bundle.
pub const MAX_FILE: u64 = 8 * 1024 * 1024;
/// The most a bundle may hold in all.
pub const MAX_TOTAL: u64 = 64 * 1024 * 1024;
/// The most files a bundle may hold.
pub const MAX_FILES: usize = 2000;
/// The deepest a path may go.
pub const MAX_DEPTH: usize = 16;

/// Why a bundle, its manifest or its archive is refused. Paths are
/// bundle-relative and cut to 200 characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the manifest is larger than 16 KiB")]
    ManifestTooLarge,
    #[error("the manifest is not valid JSON")]
    NotJson,
    #[error("the manifest names \"{0}\" twice")]
    DuplicateKey(String),
    #[error("the manifest has a field the contract does not: \"{0}\"")]
    UnknownField(String),
    #[error("the manifest has no \"{0}\"")]
    MissingField(String),
    #[error("the manifest's \"{0}\" is not valid")]
    Invalid(String),
    #[error("the scene needs scene host version {0}; this one is {HOST_VERSION}")]
    NeedsNewerHost(u32),
    #[error("not a folder or a .senatescene file")]
    NotAFolder,
    #[error("{0} is a link; scenes contain only plain files")]
    Link(String),
    #[error("{0} is not a plain file or folder")]
    Special(String),
    #[error("{0} is not a name a scene's file may have")]
    BadName(String),
    #[error("{0} is not a type of file scenes may contain")]
    Type(String),
    #[error("{0} is larger than 8 MiB")]
    FileTooLarge(String),
    #[error("the bundle is larger than 64 MiB in all")]
    TooLarge,
    #[error("the bundle has more than 2,000 files")]
    TooMany,
    #[error("there is no manifest.json at its top")]
    NoManifest,
    #[error("the manifest's entry page is not in the bundle")]
    NoEntry,
    #[error("the archive can't be used: {0}")]
    Archive(ArchiveProblem),
    #[error("{0} could not be read or written")]
    Io(String),
}

/// What is wrong with a `.senatescene` archive itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveProblem {
    #[error("it is not a readable zip archive")]
    NotZip,
    #[error("it is larger than 64 MiB")]
    Size,
    #[error("it uses zip64, which scenes don't need")]
    Zip64,
    #[error("it is encrypted")]
    Encrypted,
    #[error("its files are compressed; make it with `senate scene pack`")]
    Compressed,
    #[error("its contents don't match its directory")]
    Corrupt,
}

/// A path for a message: at most 200 characters.
pub(crate) fn shown(path: &str) -> String {
    path.chars().take(200).collect()
}

/// The types a scene may contain, by extension, and how they are served.
const TYPES: &[(&str, &str)] = &[
    ("html", "text/html"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("css", "text/css"),
    ("json", "application/json"),
    ("ndjson", "application/x-ndjson"),
    ("txt", "text/plain"),
    ("md", "text/plain"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("woff2", "font/woff2"),
];

/// Licence files without an extension, served as text.
const PLAIN_NAMES: &[&str] = &["LICENSE", "NOTICE", "COPYING", "AUTHORS"];

/// How a file at `path` (bundle-relative) is served, or `None`: not a type
/// a scene may contain.
pub fn content_type(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if PLAIN_NAMES.contains(&name) {
        return Some("text/plain");
    }
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let ext = ext.to_ascii_lowercase();
    TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// One path segment: `[A-Za-z0-9_][A-Za-z0-9._-]*`, 1–128 bytes (no dot
/// files, no `.` or `..`, no leading dash, ASCII only).
pub fn valid_segment(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0] != b'.'
        && b[0] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// A bundle-relative path: 1–16 valid segments joined by single slashes.
pub fn valid_relative(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    (1..=MAX_DEPTH).contains(&parts.len()) && parts.iter().all(|p| valid_segment(p))
}

/// Files a Finder copy or an archive leaves that are not the scene's:
/// skipped when scanning and unpacking, never served.
pub(crate) fn ignored(name: &str, at_root: bool) -> bool {
    name == ".DS_Store" || (at_root && name == "__MACOSX")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_names() {
        assert_eq!(content_type("index.html"), Some("text/html"));
        assert_eq!(content_type("a/b/x.PNG"), Some("image/png"));
        assert_eq!(content_type("LICENSE"), Some("text/plain"));
        for bad in [
            "art.svg", "x.wasm", "README", ".html", "tool.exe", "a.tar.gz",
        ] {
            assert_eq!(content_type(bad), None, "{bad}");
        }
        for good in ["index.html", "a/b.js", "_x/y-z.css"] {
            assert!(valid_relative(good), "{good}");
        }
        for bad in [
            "", "/a", "a/", "a//b", "./a", "../a", ".x", "-x", "a b", "ä.js", "a\\b",
        ] {
            assert!(!valid_relative(bad), "{bad}");
        }
        assert!(!valid_relative(&vec!["a"; 17].join("/")));
    }
}

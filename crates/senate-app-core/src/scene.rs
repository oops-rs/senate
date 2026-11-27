//! The scene bundle rules ([`senate_scene`]) for the app: the same checks
//! `senate scene inspect` applies, so what an author sees is what the app
//! accepts. Pure functions on local files; no daemon.

use std::path::Path;

/// A scene's manifest, checked.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SceneManifestInfo {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub observe_major: u32,
    pub required: Vec<String>,
    pub optional: Vec<String>,
    pub card_reads: Vec<String>,
    pub card_writes: Vec<String>,
    pub min_host_version: u32,
}

/// One file of a checked bundle.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SceneFileInfo {
    pub path: String,
    pub size: u64,
}

/// A checked bundle: everything that may ever be served from it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SceneBundleInfo {
    pub root: String,
    pub manifest: SceneManifestInfo,
    pub files: Vec<SceneFileInfo>,
    /// SHA-256 over every path and its bytes, lowercase hex.
    pub digest: String,
}

/// Why an archive itself is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SceneArchiveProblem {
    NotZip,
    Size,
    Zip64,
    Encrypted,
    Compressed,
    Corrupt,
}

/// Why a bundle, its manifest or its archive is refused (the app words
/// each in its own language).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum SceneProblem {
    #[error("manifest too large")]
    ManifestTooLarge,
    #[error("manifest not JSON")]
    NotJson,
    #[error("duplicate key {key}")]
    DuplicateKey { key: String },
    #[error("unknown field {field}")]
    UnknownField { field: String },
    #[error("missing field {field}")]
    MissingField { field: String },
    #[error("invalid field {field}")]
    Invalid { field: String },
    #[error("needs host version {version}")]
    NeedsNewerHost { version: u32 },
    #[error("not a folder")]
    NotAFolder,
    #[error("link {path}")]
    Link { path: String },
    #[error("special file {path}")]
    Special { path: String },
    #[error("bad name {path}")]
    BadName { path: String },
    #[error("disallowed type {path}")]
    Type { path: String },
    #[error("file too large {path}")]
    FileTooLarge { path: String },
    #[error("bundle too large")]
    TooLarge,
    #[error("too many files")]
    TooMany,
    #[error("no manifest")]
    NoManifest,
    #[error("no entry")]
    NoEntry,
    #[error("archive: {problem:?}")]
    Archive { problem: SceneArchiveProblem },
    #[error("io {path}")]
    Io { path: String },
}

impl From<senate_scene::Problem> for SceneProblem {
    fn from(p: senate_scene::Problem) -> Self {
        use senate_scene::{ArchiveProblem as A, Problem as P};
        match p {
            P::ManifestTooLarge => Self::ManifestTooLarge,
            P::NotJson => Self::NotJson,
            P::DuplicateKey(key) => Self::DuplicateKey { key },
            P::UnknownField(field) => Self::UnknownField { field },
            P::MissingField(field) => Self::MissingField { field },
            P::Invalid(field) => Self::Invalid { field },
            P::NeedsNewerHost(version) => Self::NeedsNewerHost { version },
            P::NotAFolder => Self::NotAFolder,
            P::Link(path) => Self::Link { path },
            P::Special(path) => Self::Special { path },
            P::BadName(path) => Self::BadName { path },
            P::Type(path) => Self::Type { path },
            P::FileTooLarge(path) => Self::FileTooLarge { path },
            P::TooLarge => Self::TooLarge,
            P::TooMany => Self::TooMany,
            P::NoManifest => Self::NoManifest,
            P::NoEntry => Self::NoEntry,
            P::Io(path) => Self::Io { path },
            P::Archive(a) => Self::Archive {
                problem: match a {
                    A::NotZip => SceneArchiveProblem::NotZip,
                    A::Size => SceneArchiveProblem::Size,
                    A::Zip64 => SceneArchiveProblem::Zip64,
                    A::Encrypted => SceneArchiveProblem::Encrypted,
                    A::Compressed => SceneArchiveProblem::Compressed,
                    A::Corrupt => SceneArchiveProblem::Corrupt,
                },
            },
        }
    }
}

/// Why one file could not be served from a bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum SceneReadError {
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

fn manifest_info(m: senate_scene::Manifest) -> SceneManifestInfo {
    SceneManifestInfo {
        name: m.name,
        version: m.version,
        entry: m.entry,
        observe_major: m.observe_major,
        required: m.required,
        optional: m.optional,
        card_reads: m.card_reads,
        card_writes: m.card_writes,
        min_host_version: m.min_host_version,
    }
}

/// The scene host contract version (`min_host_version` may not exceed it).
#[uniffi::export]
pub fn scene_host_version() -> u32 {
    senate_scene::HOST_VERSION
}

/// The largest file a bundle may hold.
#[uniffi::export]
pub fn scene_max_file() -> u64 {
    senate_scene::MAX_FILE
}

/// Parse and check a manifest's bytes.
#[uniffi::export]
pub fn scene_parse_manifest(bytes: Vec<u8>) -> Result<SceneManifestInfo, SceneProblem> {
    Ok(manifest_info(senate_scene::Manifest::parse(&bytes)?))
}

/// Check the bundle folder at `root` and everything in it. Unix only, as
/// senate_scene's folder checks are.
#[cfg(unix)]
#[uniffi::export]
pub fn scene_scan(root: String) -> Result<SceneBundleInfo, SceneProblem> {
    let bundle = senate_scene::Bundle::scan(Path::new(&root))?;
    Ok(SceneBundleInfo {
        root,
        manifest: manifest_info(bundle.manifest),
        files: bundle
            .files
            .into_iter()
            .map(|f| SceneFileInfo {
                path: f.path,
                size: f.size,
            })
            .collect(),
        digest: bundle.digest,
    })
}

/// Check an archive's central directory alone (nothing is unpacked).
#[uniffi::export]
pub fn scene_check_archive(bytes: Vec<u8>) -> Result<(), SceneProblem> {
    senate_scene::entries(&bytes)?;
    Ok(())
}

/// Unpack a `.senatescene` into the new folder `staging`; the bundle's
/// folder within it, to scan.
#[uniffi::export]
pub fn scene_unpack(archive: String, staging: String) -> Result<String, SceneProblem> {
    let root = senate_scene::unpack(Path::new(&archive), Path::new(&staging))?;
    Ok(root.to_string_lossy().into_owned())
}

/// One file of a bundle, read without following any link. Unix only.
#[cfg(unix)]
#[uniffi::export]
pub fn scene_read(root: String, path: String) -> Result<Vec<u8>, SceneReadError> {
    use senate_scene::ReadError as R;
    senate_scene::read(Path::new(&root), &path).map_err(|e| match e {
        R::Path => SceneReadError::Path,
        R::NotFound => SceneReadError::NotFound,
        R::Link => SceneReadError::Link,
        R::NotPlain => SceneReadError::NotPlain,
        R::TooLarge => SceneReadError::TooLarge,
    })
}

/// How a bundle file is served, or nil: not a type scenes may contain.
#[uniffi::export]
pub fn scene_content_type(path: String) -> Option<String> {
    senate_scene::content_type(&path).map(str::to_owned)
}

/// A valid bundle-relative path.
#[uniffi::export]
pub fn scene_valid_path(path: String) -> bool {
    senate_scene::valid_relative(&path)
}

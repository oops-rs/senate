//! `senate scene`: check and package a scene bundle for the Senate app,
//! with exactly the rules the app applies when a person adds one
//! ([`senate_scene`]). Neither command needs a daemon or touches a Senate.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use senate_scene::{Bundle, Manifest};

#[derive(Args)]
pub(super) struct Scene {
    #[command(subcommand)]
    command: SceneCmd,
}

#[derive(Subcommand)]
enum SceneCmd {
    /// Check a bundle folder or a .senatescene file and show what the scene
    /// asks for, as the app would before adding it.
    Inspect {
        /// A bundle folder (with manifest.json at its top) or a .senatescene file.
        path: PathBuf,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check a bundle folder and write it as a .senatescene file: its files
    /// only, sorted, uncompressed, with fixed times, so the same bundle
    /// always makes the same bytes.
    Pack {
        /// The bundle folder.
        dir: PathBuf,
        /// Where to write it (it must not exist). Default:
        /// `<folder name>-<version>.senatescene` in the current directory.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

pub(super) fn run(args: Scene) -> anyhow::Result<()> {
    match args.command {
        SceneCmd::Inspect { path, json } => inspect(&path, json),
        SceneCmd::Pack { dir, out } => pack(&dir, out),
    }
}

fn inspect(path: &Path, json: bool) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("{} cannot be read", path.display()))?;
    let (bundle, archive) = if meta.is_dir() {
        (Bundle::scan(path).map_err(refused)?, false)
    } else {
        // Unpacked into a private scratch folder, then checked as a folder.
        let scratch = tempfile::tempdir().context("making a scratch folder")?;
        let root = senate_scene::unpack(path, &scratch.path().join("scene")).map_err(refused)?;
        (Bundle::scan(&root).map_err(refused)?, true)
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&describe(&bundle, archive))?
        );
    } else {
        print!("{}", text(&bundle, archive));
    }
    Ok(())
}

fn pack(dir: &Path, out: Option<PathBuf>) -> anyhow::Result<()> {
    let bundle = Bundle::scan(dir).map_err(refused)?;
    let out = match out {
        Some(out) => out,
        None => {
            let base = std::fs::canonicalize(dir)?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "scene".into());
            PathBuf::from(format!("{base}-{}.senatescene", bundle.manifest.version))
        }
    };
    // Never into the bundle itself (it would not belong there).
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if let (Ok(parent), Ok(root)) = (std::fs::canonicalize(parent), std::fs::canonicalize(dir))
        && parent.starts_with(&root)
    {
        bail!(
            "{} is inside the bundle; write it somewhere else",
            out.display()
        );
    }
    if out.exists() {
        bail!("{} already exists", out.display());
    }
    let packed = senate_scene::pack(dir, &out).map_err(refused)?;
    println!(
        "Wrote {} ({} files, {} bytes of content, fingerprint {}).",
        out.display(),
        packed.files.len(),
        packed.size(),
        &packed.digest[..12]
    );
    Ok(())
}

fn refused(problem: senate_scene::Problem) -> anyhow::Error {
    anyhow::anyhow!("not a valid scene: {problem}")
}

fn none_or(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

fn text(bundle: &Bundle, archive: bool) -> String {
    let m: &Manifest = &bundle.manifest;
    let links = if m.wants_topology() {
        "asked (a separate permission)"
    } else {
        "not asked"
    };
    let contents = format!(
        "{} files, {} bytes ({})",
        bundle.files.len(),
        bundle.size(),
        if archive { "archive" } else { "folder" }
    );
    let rows = [
        ("entry", m.entry.clone()),
        ("observes", "activity metadata, no message text".to_owned()),
        ("links", links.to_owned()),
        ("requires", none_or(&m.required)),
        ("uses if offered", none_or(&m.optional)),
        ("reads cards", none_or(&m.card_reads)),
        ("may write cards", none_or(&m.card_writes)),
        ("host version", format!("{} or later", m.min_host_version)),
        ("contents", contents),
        ("fingerprint", bundle.digest.clone()),
    ];
    let mut out = format!("{} {}\n", m.name, m.version);
    for (label, value) in rows {
        out.push_str(&format!("  {label:<16} {value}\n"));
    }
    out
}

fn describe(bundle: &Bundle, archive: bool) -> serde_json::Value {
    let m = &bundle.manifest;
    serde_json::json!({
        "valid": true,
        "archive": archive,
        "name": m.name,
        "version": m.version,
        "entry": m.entry,
        "observe": { "major": m.observe_major, "required": m.required, "optional": m.optional },
        "topology": m.wants_topology(),
        "cards": { "reads": m.card_reads, "writes": m.card_writes },
        "min_host_version": m.min_host_version,
        "files": bundle.files.iter().map(|f| serde_json::json!({"path": f.path, "size": f.size})).collect::<Vec<_>>(),
        "size": bundle.size(),
        "digest": bundle.digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"name":"Fixture","version":"1.2.0","entry":"index.html","observe":{"major":1,"required":[],"optional":["topology"]},"cards":{"reads":[],"writes":[]},"min_host_version":1}"#;

    fn bundle() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("manifest.json"), MANIFEST).expect("write");
        std::fs::write(dir.path().join("index.html"), "<p>hi</p>").expect("write");
        dir
    }

    #[test]
    fn pack_then_inspect_agree() {
        let src = bundle();
        let out = tempfile::tempdir().expect("tempdir");
        let file = out.path().join("f.senatescene");
        pack(src.path(), Some(file.clone())).expect("pack");
        inspect(&file, true).expect("inspect archive");
        inspect(src.path(), false).expect("inspect folder");
        let scanned = Bundle::scan(src.path()).expect("scan");
        let text = text(&scanned, false);
        assert!(text.starts_with("Fixture 1.2.0\n"), "{text}");
        assert!(text.contains("asked (a separate permission)"), "{text}");
        assert_eq!(describe(&scanned, false)["digest"], scanned.digest.as_str());
        // Never overwrites, never writes into the bundle.
        assert!(pack(src.path(), Some(file)).is_err());
        assert!(pack(src.path(), Some(src.path().join("x.senatescene"))).is_err());
        assert!(!src.path().join("x.senatescene").exists());
    }

    #[test]
    fn an_invalid_bundle_is_refused_with_its_reason() {
        let src = bundle();
        std::fs::write(src.path().join("run.sh"), "x").expect("write");
        let error = inspect(src.path(), false).expect_err("refused");
        assert_eq!(
            error.to_string(),
            "not a valid scene: run.sh is not a type of file scenes may contain"
        );
    }
}

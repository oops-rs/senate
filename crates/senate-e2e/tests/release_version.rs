//! The release version is written in four files (docs/release.md). They
//! agree in this repository, and `deploy/bump-version.sh` moves all of
//! them together and nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The files that state the version, from the repository root.
const PLACES: [&str; 4] = [
    "Cargo.toml",
    "shim/senate/Cargo.toml",
    "apps/macos/project.yml",
    "README.md",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn bump(root: &Path, args: &[&str]) -> Output {
    Command::new("sh")
        .arg(repo().join("deploy/bump-version.sh"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A copy of the files that state the version, laid out as in the
/// repository.
fn copy_of_the_places() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for place in PLACES {
        let to = dir.path().join(place);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(repo().join(place), to).unwrap();
    }
    dir
}

fn read(root: &Path, place: &str) -> String {
    std::fs::read_to_string(root.join(place)).unwrap()
}

#[test]
fn every_place_that_states_the_version_agrees() {
    let out = bump(&repo(), &["--check"]);
    assert!(out.status.success(), "{}", said(&out));
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        said(&out).starts_with(&format!("{version} (")),
        "{}",
        said(&out)
    );
}

#[test]
fn a_bump_changes_every_place_and_only_the_version() {
    let dir = copy_of_the_places();
    let root = dir.path();
    let old = env!("CARGO_PKG_VERSION");
    let before: Vec<String> = PLACES.iter().map(|place| read(root, place)).collect();

    let out = bump(root, &["--no-lock", "9.8.7"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains(&format!("{old} -> 9.8.7")),
        "{}",
        said(&out)
    );
    let check = bump(root, &["--check"]);
    assert!(check.status.success(), "{}", said(&check));
    assert!(said(&check).starts_with("9.8.7 ("), "{}", said(&check));

    for (place, before) in PLACES.iter().zip(&before) {
        let after = read(root, place);
        // Line for line the same file, the version aside.
        assert_eq!(
            after.replace("9.8.7", old),
            *before,
            "{place} changed beyond its version"
        );
        assert!(after.contains("9.8.7"), "{place} was not changed");
    }
    assert!(read(root, "Cargo.toml").contains("\n[workspace.package]\nversion = \"9.8.7\"\n"));
    assert!(
        read(root, "Cargo.toml")
            .contains("senate-core = { path = \"crates/senate-core\", version = \"9.8.7\" }")
    );
    assert!(read(root, "apps/macos/project.yml").contains("MARKETING_VERSION: \"9.8.7\""));
    assert!(
        read(root, "README.md").contains("| [`senated`](crates/senated) | `9.8.7` |"),
        "the README's crate table follows"
    );
}

#[test]
fn a_bad_version_or_a_disagreement_changes_nothing() {
    let dir = copy_of_the_places();
    let root = dir.path();
    let before: Vec<String> = PLACES.iter().map(|place| read(root, place)).collect();
    let unchanged = |why: &str| {
        for (place, before) in PLACES.iter().zip(&before) {
            assert_eq!(read(root, place), *before, "{place} after {why}");
        }
    };
    for bad in ["1.2", "v1.2.3", "1.2.3-rc1", env!("CARGO_PKG_VERSION")] {
        let out = bump(root, &["--no-lock", bad]);
        assert!(!out.status.success(), "{bad}: {}", said(&out));
        unchanged(bad);
    }

    // One place left behind by hand: the check names it, and a bump
    // refuses to paper over it.
    let app = root.join("apps/macos/project.yml");
    let stale = read(root, "apps/macos/project.yml").replace(env!("CARGO_PKG_VERSION"), "0.0.1");
    std::fs::write(&app, &stale).unwrap();
    let check = bump(root, &["--check"]);
    assert!(!check.status.success(), "{}", said(&check));
    assert!(
        said(&check).contains("MARKETING_VERSION: \"0.0.1\""),
        "{}",
        said(&check)
    );
    let out = bump(root, &["--no-lock", "9.8.7"]);
    assert!(!out.status.success(), "{}", said(&out));
    assert_eq!(read(root, "apps/macos/project.yml"), stale);
    assert!(!read(root, "Cargo.toml").contains("9.8.7"));
}

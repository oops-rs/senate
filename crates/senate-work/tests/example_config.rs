#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `runner.example.toml` parses into the config structs.

use senate_work::{DockerRunnerConfig, GhForgeConfig, GitCredential};
use serde::Deserialize;

#[derive(Deserialize)]
struct Example {
    runner: DockerRunnerConfig,
    forge: GhForgeConfig,
}

#[test]
fn example_config_parses() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/runner.example.toml");
    let ex: Example = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(ex.runner.image, "senate-runner:dev");
    assert!(matches!(
        ex.runner.git_credential,
        GitCredential::SshKey {
            known_hosts_file: Some(_),
            ..
        }
    ));
    assert!(ex.runner.allowed_repos.allows("example/project"));
    assert!(ex.forge.allowed_repos.allows("example/project"));
    assert_eq!(ex.forge.gh_bin, std::path::PathBuf::from("gh"));
}

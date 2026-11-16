//! The shipped example configurations are what the crates read: every
//! commented-out section, uncommented, parses into the types that read it.
//! (`senated`'s own unit tests check `senated.example.toml` section by
//! section; this checks it as the binary loads it, from the file.)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use senate_bridge::BridgeConfig;
use senate_bridge::companion::CompanionConfig;
use senate_bridge::delivery::codex::CodexConfig;
use senate_core::Locale;
use senated::{ForgeSection, SenatedConfig};

fn example(name: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config")).join(name)
}

/// `# key = …` and `# [section]` lines become settings; every other line
/// stays as it is.
fn uncommented(text: &str) -> String {
    let setting = |rest: &str| {
        rest.starts_with('[')
            || rest.split_once(" = ").is_some_and(|(key, _)| {
                !key.is_empty()
                    && key.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'
                    })
            })
    };
    text.lines()
        .map(|l| match l.strip_prefix("# ") {
            Some(rest) if setting(rest) => rest,
            _ => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_senated_example_loads_as_shipped_and_in_full() {
    let path = example("senated.example.toml");
    let shipped = SenatedConfig::load(&path).unwrap();
    assert!(shipped.speaker.is_none());
    assert_eq!(shipped.server.database_url, "memory:");
    let roster = shipped.server.roster().unwrap();
    assert_eq!(
        roster
            .by_account("chat", "u_alice")
            .map(|s| s.handle.as_str()),
        Some("alice")
    );
    assert!(
        roster.agents().is_empty(),
        "agents are seated as they connect"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    let start = text.find("# [speaker]").unwrap();
    let full = format!("{}\n{}", &text[..start], uncommented(&text[start..]));
    let config = SenatedConfig::from_toml_str(&full).unwrap();
    let speaker = config.speaker.expect("[speaker]");
    assert_eq!(speaker.locale, Locale::En, "the deployment's language");
    assert_eq!(speaker.locale_of(&"refund-v2".parse().unwrap()), Locale::Zh);
    assert!(
        speaker
            .work
            .unwrap()
            .allowed_repos
            .allows("example/project")
    );
    let work = config.work.expect("[work]");
    assert!(work.runner.allowed_repos.allows("example/project"));
    assert!(matches!(&work.forge, Some(ForgeSection::Github(gh)) if gh.always_draft));
}

#[test]
fn the_bridge_example_parses_into_what_the_bridge_reads() {
    let path = example("bridge.example.toml");
    let shipped = BridgeConfig::load(&path).unwrap();
    assert_eq!(shipped.person.as_str(), "alice");
    assert_eq!(shipped.locale, Locale::En);

    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("bridge.toml");
    std::fs::write(&full, uncommented(&std::fs::read_to_string(&path).unwrap())).unwrap();
    let config = BridgeConfig::load(&full).unwrap();
    assert_eq!(config.committees, ["refund-v2".parse().unwrap()]);
    assert_eq!(config.brief_chars, 6000);
    assert_eq!(config.branches().prefix(), "feature");
    assert_eq!(config.agents.claude.as_str(), "claude");
    assert_eq!(config.agents.codex.as_str(), "codex");
    let codex = CodexConfig::load(&full).unwrap();
    assert!(codex.listen.is_some() && codex.bin.is_some());
    let companion = CompanionConfig::load(&full).unwrap();
    assert!(companion.enabled);
    assert_eq!(companion.max_secs, 300);
}

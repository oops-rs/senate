//! The shipped example configurations are what the crates read: every
//! commented-out section, uncommented, parses into the types that read it.
//! (`senated`'s own unit tests check `senated.example.toml` section by
//! section; this checks it as the binary loads it, from the file.)

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use senate_bridge::BridgeConfig;
use senate_bridge::companion::CompanionConfig;
use senate_bridge::config::Hosted;
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

/// A developer machine under a team: what `senated --machine` and the
/// `senate` command read. (Replaces the test of `bridge.example.toml`, the
/// same file under its old name.)
#[test]
fn the_machine_example_parses_into_what_the_machine_reads() {
    let path = example("senate.example.toml");
    let shipped = BridgeConfig::load(&path).unwrap();
    assert_eq!(shipped.person.as_str(), "alice");
    assert_eq!(shipped.locale, Locale::En);
    let hosted = Hosted::load(&path).unwrap();
    assert_eq!(hosted.validate(&shipped), Ok(()));
    assert!(hosted.local.is_none(), "under a team: the Senate above");
    assert_eq!(hosted.preside.len(), 1);
    assert_eq!(hosted.preside[0].agent.to_string(), "claude");

    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("senate.toml");
    std::fs::write(&full, uncommented(&std::fs::read_to_string(&path).unwrap())).unwrap();
    let config = BridgeConfig::load(&full).unwrap();
    assert_eq!(config.committees, ["refund-v2".parse().unwrap()]);
    assert_eq!(config.brief_chars, 6000);
    assert_eq!(config.branches().prefix(), "feature");
    assert_eq!(config.agents.claude.to_string(), "claude");
    assert_eq!(config.agents.codex.to_string(), "codex");
    assert_eq!(
        Hosted::load(&full).unwrap().preside[0].listen.as_deref(),
        Some("127.0.0.1:7980")
    );
    let codex = CodexConfig::load(&full).unwrap();
    assert!(codex.listen.is_some() && codex.bin.is_some());
    let companion = CompanionConfig::load(&full).unwrap();
    assert!(companion.enabled);
    assert_eq!(companion.max_secs, 300);
}

/// Alone on one machine: exactly what `senate init --local` writes.
#[test]
fn the_local_example_is_what_init_writes() {
    let path = example("local.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let config: BridgeConfig = toml::from_str(&text).unwrap();
    let hosted = Hosted::load(&path).unwrap();
    assert_eq!(hosted.validate(&config), Ok(()));
    let local = hosted.local.clone().unwrap();
    assert_eq!(local.speaker.unwrap().to_string(), "claude");
    assert!(config.server_url.is_empty(), "no Senate above");

    let written = senate_bridge::init::LocalProfile {
        person: "alice".parse().unwrap(),
        speaker: "claude".parse().unwrap(),
        locale: Locale::En,
        listen: senate_bridge::init::DEFAULT_LOCAL_LISTEN.into(),
    }
    .to_toml();
    let as_written: BridgeConfig = toml::from_str(&written).unwrap();
    assert_eq!(as_written, config);
    assert_eq!(toml::from_str::<Hosted>(&written).unwrap(), hosted);
}

//! A developer machine with a model configured in its Senate home: the
//! built-in session works the person's desk as the secretary the
//! configuration names.

#![allow(clippy::unwrap_used)]

use senate_bridge::config::Hosted;
use senate_bridge::daemon::Timing;
use senate_bridge::{BridgeConfig, Paths};
use senate_core::{Hub, Name, SenatorKind};
use senated::Machine;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_built_in_session_works_the_persons_desk_on_a_machine() {
    // The model's credential is named by a variable cargo always sets for
    // a test, so the model counts as complete; nothing here calls it.
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    std::fs::create_dir_all(paths.home()).unwrap();
    std::fs::write(
        paths.config(),
        "person = \"alice\"\nlocale = \"en\"\n\n[local]\nspeaker = \"senator::alice\"\n\
         secretaries = [\"clerk\"]\n\n[model]\nprovider = \"anthropic\"\n\
         api_key_env = \"CARGO_MANIFEST_DIR\"\n",
    )
    .unwrap();
    std::fs::write(paths.home().join("secretary.toml"), "name = \"clerk\"\n").unwrap();
    let config = BridgeConfig::load_unchecked(&paths.config()).unwrap();
    let hosted = Hosted::load(&paths.config()).unwrap();
    let machine = Machine::start_with(paths.clone(), config, hosted, Timing::default())
        .await
        .unwrap();
    let senate = machine.senate().unwrap();
    let roster = senate.daemon.hub.roster().await.unwrap();
    let office = roster.office().unwrap();
    assert_eq!(office.speaker.to_string(), "senator::alice");
    assert_eq!(office.kind, SenatorKind::Person);
    assert_eq!(office.built_in.as_ref().map(|n| n.as_str()), Some("clerk"));
    assert!(office.triages(), "the built-in session judges roots there");
    assert_eq!(
        roster.owner_of(&"clerk".parse().unwrap()),
        Some(&Name::new("alice").unwrap())
    );
    machine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_model_no_built_in_session_runs_on_a_machine() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    std::fs::create_dir_all(paths.home()).unwrap();
    std::fs::write(
        paths.config(),
        "person = \"alice\"\n\n[local]\nspeaker = \"senator::alice\"\nsecretaries = [\"claude\"]\n",
    )
    .unwrap();
    let config = BridgeConfig::load_unchecked(&paths.config()).unwrap();
    let hosted = Hosted::load(&paths.config()).unwrap();
    let machine = Machine::start_with(paths, config, hosted, Timing::default())
        .await
        .unwrap();
    let office = machine
        .senate()
        .unwrap()
        .daemon
        .hub
        .roster()
        .await
        .unwrap()
        .office()
        .unwrap();
    assert_eq!(office.built_in, None);
    assert_eq!(
        office.desk,
        ["claude".parse::<senate_core::SenatorName>().unwrap()]
    );
    assert!(!office.triages());
    machine.shutdown().await;
}

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

/// `senate status`'s view, once the daemon has made its state.
async fn collect(paths: &Paths) -> senate_bridge::status::StatusView {
    for _ in 0..100 {
        if let Ok(view) = senate_bridge::status::collect(paths).await {
            return view;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    senate_bridge::status::collect(paths).await.unwrap()
}

/// A broken `secretary.toml` or `<committee>.secretary.toml` is listed by
/// `senate status` (file, line, message) while it is broken, with no model
/// configured and nothing having acted on the committee, and is gone when
/// it is fixed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn senate_status_lists_a_broken_secretary_file_until_it_is_fixed() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    std::fs::create_dir_all(paths.home()).unwrap();
    std::fs::write(
        paths.config(),
        "person = \"alice\"\nlocale = \"en\"\n\n[local]\nspeaker = \"senator::alice\"\n",
    )
    .unwrap();
    let secretary = paths.home().join("secretary.toml");
    let committee = paths.home().join("refund-v2.secretary.toml");
    std::fs::write(&secretary, "# mine\nmode = \"loud\"\n").unwrap();
    let config = BridgeConfig::load_unchecked(&paths.config()).unwrap();
    let hosted = Hosted::load(&paths.config()).unwrap();
    let timing = Timing {
        sweep: std::time::Duration::from_millis(50),
        ..Timing::default()
    };
    let machine = Machine::start_with(paths.clone(), config, hosted, timing)
        .await
        .unwrap();

    // Right after start: the Senate-wide file, on its line.
    let status = collect(&paths).await;
    assert_eq!(status.secretary_problems.len(), 1, "{status:?}");
    assert_eq!(status.secretary_problems[0].file, secretary);
    assert_eq!(status.secretary_problems[0].line, Some(2));
    let text = senate_bridge::status::render(&status, chrono::Utc::now());
    assert!(text.contains("secretary.toml:2: "), "{text}");

    // A committee's file that breaks later is picked up without anything
    // acting on that committee.
    std::fs::write(&committee, "mode = \n").unwrap();
    let mut lines = Vec::new();
    for _ in 0..100 {
        let status = collect(&paths).await;
        lines = status.secretary_problems;
        if lines.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|p| p.file == committee && p.line == Some(1))
    );

    // Fixed: gone.
    std::fs::write(&secretary, "answer_fallback_secs = 30\n").unwrap();
    std::fs::remove_file(&committee).unwrap();
    for _ in 0..100 {
        let status = collect(&paths).await;
        if status.secretary_problems.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let status = collect(&paths).await;
    assert!(status.secretary_problems.is_empty(), "{status:?}");
    machine.shutdown().await;
}

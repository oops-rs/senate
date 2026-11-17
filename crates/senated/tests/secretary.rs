//! The built-in session as `senated --config` configures it: `[model]` in
//! senated.toml, `secretary.toml` beside it, the session running only when
//! a model is configured and the roster refers to it, `Me` saying who the
//! Speaker is and who works its desk, the older `[speaker]` still starting,
//! and `senated secretary show`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use senate_core::SenatorKind;
use senate_core::api::Me;
use senate_speaker::ScriptedBrain;
use senated::{Assembly, AssemblyError, Senated, SenatedConfig};

const PEOPLE: &str = r#"
[[roster.principals]]
handle = "wendell"

[[tokens]]
person = "wendell"
token = "t-wendell"
"#;

/// A model that needs nothing from the environment; the Brain is scripted
/// anyway, so nothing ever reaches a model.
const MODEL: &str = "\n[model]\nmodel = \"scripted-model\"\n";

/// `senated.toml` (with `body` after the server's keys) and `files` beside
/// it, in a temporary directory.
fn home(body: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("senated.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\ndatabase_url = \"memory:\"\ndata_dir = \"{}\"\nlocale = \"en\"\n{body}",
            dir.path().join("data").display()
        ),
    )
    .unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    (dir, config)
}

async fn start(config: &Path) -> Result<Senated, AssemblyError> {
    Assembly::new(SenatedConfig::load(config).unwrap())
        .config_file(config)
        .brain(ScriptedBrain::new())
        .start()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_presides_and_the_built_in_session_is_their_secretary() {
    let (_dir, config) = home(
        &format!(
            "[roster]\nspeaker = \"senator::wendell\"\n[roster.secretaries]\nwendell = [\"clerk\"]\n{PEOPLE}{MODEL}"
        ),
        &[("secretary.toml", "name = \"clerk\"\n")],
    );
    let daemon = start(&config).await.unwrap();
    assert!(daemon.has_speaker());
    assert_eq!(daemon.built_in_session().map(|n| n.as_str()), Some("clerk"));
    let me = me(&daemon.base_url(), "t-wendell").await;
    let office = me.office.expect("an office");
    assert_eq!(office.speaker.to_string(), "senator::wendell");
    assert_eq!(office.kind, SenatorKind::Person);
    let desk: Vec<&str> = office.desk.iter().map(|n| n.as_str()).collect();
    assert_eq!(desk, ["clerk"], "the built-in session works the desk");
    assert_eq!(office.built_in.as_ref().map(|n| n.as_str()), Some("clerk"));
    assert!(me.triaged, "the built-in session judges roots");
    daemon.settle_speaker().await;
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_built_in_session_presides_over_an_unattended_root() {
    let (_dir, config) = home(
        &format!("[roster]\nspeaker = \"senator::clerk\"\n{PEOPLE}{MODEL}"),
        &[("secretary.toml", "name = \"clerk\"\n")],
    );
    let daemon = start(&config).await.unwrap();
    assert_eq!(daemon.built_in_session().map(|n| n.as_str()), Some("clerk"));
    let me = me(&daemon.base_url(), "t-wendell").await;
    let office = me.office.expect("an office");
    assert_eq!(office.speaker.to_string(), "senator::clerk");
    assert_eq!(office.kind, SenatorKind::BuiltIn);
    let desk: Vec<&str> = office.desk.iter().map(|n| n.as_str()).collect();
    assert_eq!(desk, ["clerk"]);
    assert!(me.triaged);
    daemon.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_built_in_session_named_speaker_without_a_model_is_refused() {
    let (_dir, config) = home(
        &format!("[roster]\nspeaker = \"senator::clerk\"\n{PEOPLE}"),
        &[("secretary.toml", "name = \"clerk\"\n")],
    );
    let Err(refused) = start(&config).await else {
        panic!("started without a model");
    };
    assert!(
        matches!(refused, AssemblyError::BuiltInWithoutModel(_)),
        "{refused}"
    );
    let text = refused.to_string();
    assert!(text.contains("[senator::clerk]"), "{text}");
    assert!(text.contains("[model]"), "{text}");

    // A model that is not complete is no model either; the message says
    // why.
    let (_dir, config) = home(
        &format!(
            "[roster]\nspeaker = \"senator::clerk\"\n{PEOPLE}\n[model]\nprovider = \"anthropic\"\napi_key_env = \"SENATE_TEST_NEVER_SET_6F1C\"\n"
        ),
        &[("secretary.toml", "name = \"clerk\"\n")],
    );
    let Err(refused) = start(&config).await else {
        panic!("started with an incomplete model");
    };
    assert!(
        refused.to_string().contains("SENATE_TEST_NEVER_SET_6F1C"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_nothing_refers_to_runs_nothing() {
    let (_dir, config) = home(
        &format!("[roster]\nspeaker = \"senator::wendell\"\n{PEOPLE}{MODEL}"),
        &[],
    );
    let daemon = start(&config).await.unwrap();
    assert!(!daemon.has_speaker());
    let me = me(&daemon.base_url(), "t-wendell").await;
    assert!(me.office.expect("an office").desk.is_empty());
    assert!(!me.triaged);
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_speaker_table_with_model_keys_still_starts_with_its_notes() {
    let (_dir, config) = home(
        &format!(
            "{PEOPLE}\n[speaker]\nmodel = \"scripted-model\"\nbrain_deadline_secs = 20\nanswer_fallback_secs = 30\n"
        ),
        &[],
    );
    let daemon = start(&config).await.unwrap();
    assert!(daemon.has_speaker(), "the Speaker of an unattended root");
    assert_eq!(
        daemon.built_in_session().map(|n| n.as_str()),
        Some("secretary")
    );
    let notes = daemon.secretary().notes().join("\n");
    assert!(notes.contains("[speaker] table"), "{notes}");
    assert!(
        notes.contains("read as if it were secretary.toml"),
        "{notes}"
    );
    assert!(notes.contains("top-level [model]"), "{notes}");
    let effective = daemon.secretary().refresh(None);
    assert_eq!(effective.speaker.answer_fallback_secs, 30);
    assert_eq!(effective.speaker.brain_deadline_secs, 20);
    assert_eq!(
        effective.model.clone().unwrap().model.as_deref(),
        Some("scripted-model")
    );
    let me = me(&daemon.base_url(), "t-wendell").await;
    let office = me.office.expect("an office");
    assert_eq!(office.speaker.to_string(), "senator::secretary");
    assert_eq!(office.kind, SenatorKind::BuiltIn);
    daemon.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secretary_toml_wins_over_an_old_speaker_table_and_says_so() {
    let (_dir, config) = home(
        &format!("{PEOPLE}\n[speaker]\nanswer_fallback_secs = 30\n"),
        &[("secretary.toml", "confirmation_ttl_secs = 90\n")],
    );
    let daemon = start(&config).await.unwrap();
    let effective = daemon.secretary().refresh(None);
    assert_eq!(
        effective.speaker.answer_fallback_secs, 60,
        "the table is not read"
    );
    assert_eq!(effective.speaker.confirmation_ttl_secs, 90);
    let notes = daemon.secretary().notes().join("\n");
    assert!(notes.contains("the file wins"), "{notes}");
    daemon.shutdown().await.unwrap();
}

/// `senated`, isolated from the environment running the tests.
fn senated(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_senated"));
    senate_bridge::testenv::isolate(&mut cmd, root);
    cmd
}

#[test]
fn secretary_show_names_each_source_and_never_the_key() {
    let secret = std::env::var("HOME").expect("HOME is set");
    let (dir, config) = home(
        &format!(
            "{PEOPLE}\n[model]\nprovider = \"anthropic\"\napi_key_env = \"HOME\"\nmodel = \"m-1\"\n"
        ),
        &[
            ("secretary.toml", "name = \"clerk\"\nmode = \"agent\"\n"),
            (
                "refund-v2.secretary.toml",
                "confirmation_ttl_secs = 30\n[model]\nmodel = \"m-2\"\n",
            ),
        ],
    );
    let output = senated(config.parent().unwrap())
        .args(["secretary", "show", "--config"])
        .arg(&config)
        .args(["--committee", "refund-v2"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        !text.contains(&secret),
        "the key's value is printed:\n{text}"
    );
    let line_of = |key: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key} in:\n{text}"))
            .to_owned()
    };
    let main = format!("{} [model]", config.display());
    assert!(line_of("provider").contains(&main), "{text}");
    assert!(line_of("api_key_env").contains("HOME (set)"), "{text}");
    assert!(line_of("api_key_env").contains(&main), "{text}");
    let committee_file = dir.path().join("refund-v2.secretary.toml");
    assert!(
        line_of("model").contains(&committee_file.display().to_string()),
        "{text}"
    );
    assert!(
        line_of("confirmation_ttl_secs").contains(&committee_file.display().to_string()),
        "{text}"
    );
    assert!(line_of("mode").contains("secretary.toml"), "{text}");
    assert!(text.contains("[senator::clerk]"), "{text}");
    assert!(text.contains("[refund-v2]"), "{text}");
    assert!(text.contains("No problems."), "{text}");
}

#[test]
fn secretary_show_on_a_machine_home_reads_its_language_and_its_problems() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("senate.toml"),
        "person = \"wendell\"\nlocale = \"zh\"\n\n[model]\nprovider = \"anthropic\"\napi_key_env = \"SENATE_TEST_NEVER_SET_6F1C\"\n",
    )
    .unwrap();
    std::fs::write(home.path().join("secretary.toml"), "# x\nmode = \"loud\"\n").unwrap();
    let output = senated(home.path())
        .args(["secretary", "show", "--home"])
        .arg(home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("内置会话 [senator::secretary]"), "{text}");
    assert!(
        text.contains("SENATE_TEST_NEVER_SET_6F1C（未设置）"),
        "{text}"
    );
    assert!(text.contains("问题："), "{text}");
    let secretary_file = home.path().join("secretary.toml");
    assert!(
        text.contains(&format!("{}:2:", secretary_file.display())),
        "the file and the line:\n{text}"
    );
}

// ----- `GET /v1/me` -----------------------------------------------------------------

async fn me(base: &str, token: &str) -> Me {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /v1/me HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();
    let json = body
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("no JSON in {raw}"));
    serde_json::from_str(json).unwrap()
}

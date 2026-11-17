//! A machine under a parent, at the daemon's level: `[local]` together with
//! `server_url` and `token` hosts the machine's Senate and registers it with
//! the Senate above as a child. A Senate above that does not answer at
//! start leaves the machine's Senate with no parent, and the machine says
//! so instead of failing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use senate_bridge::client::ApiClient;
use senate_bridge::config::Hosted;
use senate_bridge::daemon::Timing;
use senate_bridge::ipc::{Registration, UplinkStatus};
use senate_bridge::{BridgeConfig, Paths};
use senate_core::Locale;
use senated::{Assembly, Machine, SenatedConfig};

/// The Senate above: `acme`, alice and bob its people, no built-in session.
async fn acme() -> senated::Senated {
    let config = SenatedConfig::from_toml_str(
        r#"
listen = "127.0.0.1:0"
database_url = "memory:"
name = "acme"

[roster]
speaker = "senator::bob"

[[roster.principals]]
handle = "alice"

[[roster.principals]]
handle = "bob"

[[tokens]]
person = "alice"
token = "tok-alice"

[[tokens]]
person = "bob"
token = "tok-bob"
"#,
    )
    .unwrap();
    Assembly::new(config)
        .without_speaker()
        .start()
        .await
        .unwrap()
}

fn quick() -> Timing {
    Timing {
        sweep: Duration::from_millis(200),
        ..Timing::default()
    }
}

/// What the machine's uplink last wrote, once it is no longer pending.
async fn registration(paths: &Paths) -> UplinkStatus {
    let dir = paths.hosted("top");
    for _ in 0..200 {
        if let Some(status) = UplinkStatus::read(&dir)
            && status.registration != Registration::Pending
        {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the uplink never wrote where the registration stands");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_under_a_parent_hosts_its_senate_and_registers_it_there() {
    let above = acme().await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    let text = format!(
        "person = \"alice\"\nserver_url = \"{}\"\ntoken = \"tok-alice\"\n[local]\nsecretaries = [\"claude\"]\n",
        above.base_url()
    );
    let config: BridgeConfig = toml::from_str(&text).unwrap();
    let hosted: Hosted = toml::from_str(&text).unwrap();
    let machine = Machine::start_with(paths.clone(), config, hosted, quick())
        .await
        .unwrap();

    // Its own Senate, under acme.
    let top = machine.senate().unwrap();
    assert_eq!(top.daemon.hub.senate_path().to_string(), "/acme/alice-desk");
    let office = top.daemon.hub.office().unwrap();
    assert_eq!(office.speaker.to_string(), "/acme::senator::alice");
    assert_eq!(
        office.desk,
        ["claude".parse::<senate_core::SenatorName>().unwrap()]
    );
    // The developer-machine side serves it, with acme as its uplink.
    let bridge = machine.bridge_config();
    assert_eq!(bridge.server_url, top.url);
    let uplink = bridge.uplink.as_ref().expect("an uplink");
    assert_eq!(uplink.url, above.base_url());
    assert!(!format!("{bridge:?}").contains("tok-alice"), "redacted");

    // Registered above, alice its Speaker.
    let status = registration(&paths).await;
    assert_eq!(status.registration, Registration::Registered);
    assert_eq!(status.speaker.to_string(), "/acme::senator::alice");
    let children = ApiClient::new(&above.base_url(), "tok-bob")
        .unwrap()
        .children()
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].name.as_str(), "alice-desk");
    assert_eq!(children[0].speaker.as_str(), "alice");
    machine.shutdown().await;

    // A restart registers again, and keeps its cursors.
    let config: BridgeConfig = toml::from_str(&text).unwrap();
    let hosted: Hosted = toml::from_str(&text).unwrap();
    std::fs::remove_file(paths.hosted("top").join(UplinkStatus::FILE)).unwrap();
    let again = Machine::start_with(paths.clone(), config, hosted, quick())
        .await
        .unwrap();
    assert_eq!(
        registration(&paths).await.registration,
        Registration::Registered
    );
    again.shutdown().await;
    above.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parent_that_does_not_answer_leaves_the_senate_with_no_parent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    let text = "person = \"alice\"\nserver_url = \"http://127.0.0.1:1\"\ntoken = \"t\"\n[local]\n";
    let config: BridgeConfig = toml::from_str(text).unwrap();
    let hosted: Hosted = toml::from_str(text).unwrap();
    let machine = Machine::start_with(paths.clone(), config, hosted, quick())
        .await
        .expect("it starts all the same");
    let top = machine.senate().unwrap();
    assert_eq!(top.daemon.hub.parent(), None);
    assert_eq!(top.daemon.hub.senate_path().to_string(), "/alice-desk");
    let status = registration(&paths).await;
    assert!(
        matches!(status.registration, Registration::Unreachable { .. }),
        "{status:?}"
    );
    machine.shutdown().await;

    for locale in [Locale::En, Locale::Zh] {
        let text = senated::machine::parent_unreachable("http://127.0.0.1:1", "refused", locale);
        assert!(
            text.contains("http://127.0.0.1:1") && text.contains("Senate"),
            "{text}"
        );
    }
    assert!(senated::machine::parent_unreachable("u", "e", Locale::En).contains("did not answer"));
    assert!(senated::machine::parent_unreachable("u", "e", Locale::Zh).contains("没有应答"));
}

/// The wait between tries to register grows from the poll period and stops
/// at a ceiling.
#[test]
fn the_wait_between_tries_to_register_grows_to_a_ceiling() {
    use senated::machine::uplink::{MAX_BACKOFF, next_backoff};
    let poll = Duration::from_secs(2);
    let mut wait = Duration::ZERO;
    let mut seen = Vec::new();
    for _ in 0..8 {
        wait = next_backoff(wait, poll);
        seen.push(wait.as_secs());
    }
    assert_eq!(seen, [2, 4, 8, 16, 32, 60, 60, 60]);
    assert_eq!(next_backoff(MAX_BACKOFF, poll), MAX_BACKOFF);
    // A poll period longer than the ceiling is the wait.
    let slow = Duration::from_secs(90);
    assert_eq!(next_backoff(Duration::ZERO, slow), slow);
}

/// A parent that is not up when the machine starts: the machine's Senate
/// starts with no parent and keeps trying; when the parent comes up it
/// registers, and its `/v1/me` and `senate status` name the parent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parent_that_comes_up_after_the_child_is_learned_and_registered_with() {
    // A port nobody listens on yet.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("h"));
    let text = format!(
        "person = \"alice\"\nserver_url = \"http://127.0.0.1:{port}\"\ntoken = \"tok-alice\"\n[local]\n"
    );
    let config: BridgeConfig = toml::from_str(&text).unwrap();
    let hosted: Hosted = toml::from_str(&text).unwrap();
    let machine = Machine::start_with(paths.clone(), config, hosted, quick())
        .await
        .expect("it starts all the same");
    let top = machine.senate().unwrap();
    assert_eq!(top.daemon.hub.parent(), None);
    assert!(matches!(
        registration(&paths).await.registration,
        Registration::Unreachable { .. }
    ));
    let me = |client: &ApiClient| {
        let client = client.clone();
        async move { client.me().await.unwrap() }
    };
    let here = ApiClient::new(&top.url, &top.token).unwrap();
    assert_eq!(me(&here).await.parent, None);

    // The Senate above comes up, on the address the child was told.
    let config = SenatedConfig::from_toml_str(&format!(
        r#"
listen = "127.0.0.1:{port}"
database_url = "memory:"
name = "acme"

[roster]
speaker = "senator::bob"

[[roster.principals]]
handle = "alice"

[[roster.principals]]
handle = "bob"

[[tokens]]
person = "alice"
token = "tok-alice"

[[tokens]]
person = "bob"
token = "tok-bob"
"#
    ))
    .unwrap();
    let above = Assembly::new(config)
        .without_speaker()
        .start()
        .await
        .unwrap();

    // Registered, with no restart and the local Senate never blocked.
    let mut registered = false;
    for _ in 0..200 {
        let status = registration(&paths).await;
        if status.registration == Registration::Registered {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(registered, "the uplink never registered");
    assert_eq!(top.daemon.hub.parent().unwrap().to_string(), "/acme");
    assert_eq!(top.daemon.hub.senate_path().to_string(), "/acme/alice-desk");
    let learned = me(&here).await;
    assert_eq!(learned.parent.clone().unwrap().to_string(), "/acme");
    assert_eq!(learned.senate_path().to_string(), "/acme/alice-desk");
    let children = ApiClient::new(&above.base_url(), "tok-bob")
        .unwrap()
        .children()
        .await
        .unwrap();
    assert_eq!(children.len(), 1);

    // `senate status` asks the daemon, which asks the Senate again.
    let mut senate = String::new();
    for _ in 0..100 {
        let view = senate_bridge::status::collect(&paths).await;
        if let Some(path) = view.ok().and_then(|v| v.senate) {
            senate = path.to_string();
            if senate == "/acme/alice-desk" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(senate, "/acme/alice-desk");
    machine.shutdown().await;
    above.shutdown().await.unwrap();
}

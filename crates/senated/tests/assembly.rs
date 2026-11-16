//! The assembly as a private layer uses it: its own authentication and
//! people, a component of its own on the Hub, and the refusals that keep a
//! daemon from starting half-assembled.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use senate_core::{
    Account, Address, ChatSurface, CommitteeId, Kind, Name, NewEvent, Roster, Senior, Seq,
};
use senate_speaker::{ScriptedBrain, SpeakerConfig};
use senated::{
    Assembly, AssemblyError, Authenticate, ChatMount, ComponentContext, NoChat, SenatedConfig,
    component_fn,
};

fn config() -> SenatedConfig {
    // Nothing about people here: the layer brings its own.
    SenatedConfig::from_toml_str("listen = \"127.0.0.1:0\"\ndatabase_url = \"memory:\"\n").unwrap()
}

/// A layer's own identity system: one opaque token per person.
struct Directory;

#[async_trait]
impl Authenticate for Directory {
    async fn authenticate(&self, token: &str) -> Option<Name> {
        (token == "sso:dana").then(|| Name::new("dana").unwrap())
    }
}

fn people() -> Roster {
    let mut dana = Senior::new(Name::new("dana").unwrap());
    dana.accounts.push(Account {
        platform: "chat".into(),
        id: "u_dana".into(),
    });
    Roster::top(vec![dana], Vec::new()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_layer_brings_its_own_people_tokens_and_components() {
    let stopped = Arc::new(AtomicBool::new(false));
    let saw_stop = stopped.clone();
    // An entry point of the layer's own: it writes through the Hub, then
    // waits for shutdown.
    let entry = component_fn("entry-point", move |ctx: ComponentContext| {
        let stopped = saw_stop.clone();
        async move {
            let committee = CommitteeId::new("intake").unwrap();
            ctx.hub
                .append(NewEvent::new(
                    committee,
                    Kind::Note,
                    Address::senior(&Name::new("dana").unwrap()),
                    "opened by the entry point",
                ))
                .await?;
            ctx.shutdown.cancelled().await;
            stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    let daemon = Assembly::new(config())
        .auth(Arc::new(Directory))
        .roster(people())
        .speaker(SpeakerConfig::new("/nonexistent/speaker"))
        .brain(ScriptedBrain::new())
        .component(entry)
        .start()
        .await
        .unwrap();
    assert!(daemon.has_speaker());

    let me = reqwest_get(&daemon.base_url(), "sso:dana").await;
    assert_eq!(me["person"], "dana");
    assert_eq!(reqwest_status(&daemon.base_url(), "dev-token").await, 401);

    let committee = CommitteeId::new("intake").unwrap();
    let hub = daemon.hub();
    let mut written = Vec::new();
    for _ in 0..200 {
        written = hub.read(&committee, Seq::ZERO, 10).await.unwrap();
        if !written.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(written.len(), 1, "the component wrote through the hub");
    daemon.settle_speaker().await;
    daemon.shutdown().await.unwrap();
    assert!(stopped.load(Ordering::SeqCst), "told to stop, and stopped");
}

async fn reqwest_get(base: &str, token: &str) -> serde_json::Value {
    let response = request(base, token).await;
    serde_json::from_str(&response.1).unwrap()
}

async fn reqwest_status(base: &str, token: &str) -> u16 {
    request(base, token).await.0
}

/// `GET /v1/me` over a plain TCP connection: (status, body).
async fn request(base: &str, token: &str) -> (u16, String) {
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
    let status = raw[9..12].parse().unwrap();
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();
    // A chunked body: the JSON is the one line that starts with `{`.
    let body = body
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_default()
        .to_owned();
    (status, body)
}

fn mount() -> ChatMount {
    let surface: Arc<dyn ChatSurface> = Arc::new(NoChat);
    ChatMount::new(surface).0
}

#[tokio::test]
async fn a_half_assembled_daemon_does_not_start() {
    let refused = Assembly::new(config())
        .roster(people())
        .chat(mount())
        .start()
        .await;
    assert!(matches!(refused, Err(AssemblyError::SurfaceWithoutSpeaker)));

    let refused = Assembly::new(config())
        .roster(people())
        .speaker(SpeakerConfig::new("/nonexistent/speaker"))
        .brain(ScriptedBrain::new())
        .chat(mount())
        .chat(mount())
        .start()
        .await;
    assert!(matches!(refused, Err(AssemblyError::SeveralSurfaces(2))));

    let refused = Assembly::new(
        SenatedConfig::from_toml_str("listen = \"127.0.0.1:0\"\ndatabase_url = \"nowhere:\"\n")
            .unwrap(),
    )
    .start()
    .await;
    assert!(matches!(refused, Err(AssemblyError::Store { .. })));
}

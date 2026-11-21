//! Conformance of the chat gateway protocol (docs/chat-gateway-ipc.md): a
//! fake adapter on the real socket, served the way `senated` serves it, and
//! every `ChatSurface` method driven through the surface the Speaker gets.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use senate_core::{
    Card, ChatSurface, CreatedGroup, Link, Locale, NewGroup, PostTarget, StatusState, Step,
    SurfaceError,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::bind::InboundHandler;
use super::transport::{self, RemoteSurface};
use crate::{Assembly, ChatGateway, SenatedConfig};

const TOKEN: &str = "conformance-token";
const EVERY_OP: [&str; 10] = [
    "post",
    "post_mentioning",
    "post_card",
    "update_card",
    "pin",
    "create_group",
    "invite",
    "rename_group",
    "adopt_group",
    "release_group",
];

/// Every call the adapter received, as `(op, args)`.
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A platform adapter written from the protocol document alone. It keeps
/// its own idempotency table keyed by `operation_id`.
async fn fake_adapter(path: &Path, capabilities: &[&str], calls: Calls) {
    let stream = loop {
        match UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    };
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let hello = json!({"type":"hello","version":1,"platform":"chat","token":TOKEN,"capabilities":capabilities});
    write
        .write_all(format!("{hello}\n").as_bytes())
        .await
        .unwrap();
    let ready: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ready, json!({"type":"ready","version":1}));

    let mut done: HashMap<String, Value> = HashMap::new();
    let mut serial = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "call");
        let op = frame["op"].as_str().unwrap().to_owned();
        let args = frame["args"].clone();
        calls.lock().unwrap().push((op.clone(), args.clone()));
        let key = args["operation_id"].as_str().map(str::to_owned);
        let value = match key.as_ref().and_then(|key| done.get(key)) {
            // A replayed operation: the original outcome, marked as such.
            Some(first) => {
                let mut first = first.clone();
                if first.get("duplicate").is_some() {
                    first["duplicate"] = json!(true);
                }
                first
            }
            None => {
                serial += 1;
                let value = match op.as_str() {
                    "post" | "post_mentioning" | "post_card" => json!({
                        "message_ref": format!("om_{serial}"),
                        "thread_ref": null,
                        "duplicate": false,
                    }),
                    "create_group" => json!({
                        "group_id": format!("oc_{serial}"),
                        "not_added": args["group"]["invite"].as_array().unwrap()
                            .iter().filter(|u| *u == "u_stranger").collect::<Vec<_>>(),
                    }),
                    "adopt_group" => json!({"group_id":"oc_bound","already_granted":false}),
                    "release_group" => json!(true),
                    "rename_group" if args["name"] == "" => {
                        let error = json!({"kind":"rejected","detail":"empty name"});
                        let reply = json!({"type":"result","id":frame["id"],"error":error});
                        write
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .unwrap();
                        continue;
                    }
                    _ => Value::Null,
                };
                if let Some(key) = key {
                    done.insert(key, value.clone());
                }
                value
            }
        };
        let reply = json!({"type":"result","id":frame["id"],"value":value});
        write
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    }
}

struct Served {
    surface: Arc<dyn ChatSurface>,
    calls: Calls,
    shutdown: CancellationToken,
    server: tokio::task::JoinHandle<Result<()>>,
    _dir: tempfile::TempDir,
}

/// `senated`'s side as `Assembly` wires it for a `[chat_gateway]`: the
/// remote surface inside a `ChatGateway`, served on a Unix socket.
async fn serve(capabilities: &'static [&'static str]) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let config = SenatedConfig::from_toml_str(&format!(
        "listen = \"127.0.0.1:0\"\ndatabase_url = \"sqlite:{}\"\n",
        dir.path().join("log.db").display(),
    ))
    .unwrap();
    let store = senate_log::connect(&config.server.database_url)
        .await
        .unwrap();
    let daemon = Assembly::new(config)
        .store(store.clone())
        .start()
        .await
        .unwrap();
    let remote = RemoteSurface::new("chat".into());
    let gateway = ChatGateway::new(remote.clone());
    let surface = gateway.mount().unwrap().surface;
    let handler = Arc::new(InboundHandler::new(
        gateway,
        daemon.hub(),
        store,
        "chat".into(),
        surface.clone(),
        Locale::En,
        false,
    ));
    let path = dir.path().join("gateway.sock");
    let shutdown = CancellationToken::new();
    let server = {
        let (path, shutdown) = (path.clone(), shutdown.clone());
        tokio::spawn(async move {
            let _daemon = daemon;
            transport::serve(&path, TOKEN, remote, handler, shutdown).await
        })
    };
    let calls = Calls::default();
    tokio::spawn({
        let calls = calls.clone();
        async move { fake_adapter(&path, capabilities, calls).await }
    });
    // Wait for the adapter's hello to attach the surface.
    timeout(Duration::from_secs(5), async {
        while matches!(
            surface.release_group("probe").await,
            Err(SurfaceError::Unavailable(_))
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the adapter attaches");
    calls.lock().unwrap().clear();
    Served {
        surface,
        calls,
        shutdown,
        server,
        _dir: dir,
    }
}

impl Served {
    fn last(&self) -> (String, Value) {
        self.calls.lock().unwrap().last().cloned().unwrap()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.server.await.unwrap().unwrap();
    }
}

fn card() -> Card {
    Card {
        title: "Refund retries".into(),
        state: StatusState::Processing,
        markdown: "asked by **alice**".into(),
        steps: vec![Step {
            id: "read".into(),
            label: "read the log".into(),
            done: true,
        }],
        links: vec![Link {
            label: "branch".into(),
            url: "https://example.test/b".into(),
        }],
    }
}

#[tokio::test]
async fn an_adapter_with_every_op_serves_every_surface_method() {
    let served = serve(&EVERY_OP).await;
    let surface = &served.surface;
    let root = PostTarget::NewRoot {
        group_id: "oc_agents".into(),
    };
    let reply = PostTarget::Reply {
        message_ref: "om_ask".into(),
        in_thread: true,
    };

    let posted = surface
        .post(root.clone(), "hello", "op-post")
        .await
        .unwrap();
    assert!(!posted.duplicate);
    assert_eq!(
        served.last(),
        (
            "post".into(),
            json!({"target":{"kind":"new_root","group_id":"oc_agents"},"markdown":"hello","operation_id":"op-post"})
        )
    );

    let mentioned = surface
        .post_mentioning(reply.clone(), "your turn", "u_alice", "op-mention")
        .await
        .unwrap();
    assert!(mentioned.message_ref.starts_with("om_"));
    assert_eq!(
        served.last(),
        (
            "post_mentioning".into(),
            json!({"target":{"kind":"reply","message_ref":"om_ask","in_thread":true},"markdown":"your turn","user":"u_alice","operation_id":"op-mention"})
        )
    );

    let wire_card = json!({
        "title":"Refund retries","state":"processing","markdown":"asked by **alice**",
        "steps":[{"id":"read","label":"read the log","done":true}],
        "links":[{"label":"branch","url":"https://example.test/b"}],
    });
    let carded = surface
        .post_card(root.clone(), &card(), "op-card")
        .await
        .unwrap();
    assert_eq!(
        served.last(),
        (
            "post_card".into(),
            json!({"target":{"kind":"new_root","group_id":"oc_agents"},"card":wire_card,"operation_id":"op-card"})
        )
    );

    let mut done = card();
    done.state = StatusState::Archived;
    surface
        .update_card(&carded.message_ref, &done, "op-update")
        .await
        .unwrap();
    let (op, args) = served.last();
    assert_eq!(op, "update_card");
    assert_eq!(args["message_ref"], json!(carded.message_ref));
    assert_eq!(args["card"]["state"], "archived");
    assert_eq!(args["operation_id"], "op-update");

    surface.pin(&carded.message_ref, "op-pin").await.unwrap();
    assert_eq!(
        served.last(),
        (
            "pin".into(),
            json!({"message_ref":carded.message_ref,"operation_id":"op-pin"})
        )
    );

    let group = NewGroup {
        name: "refund · agents".into(),
        invite: vec!["u_alice".into(), "u_stranger".into()],
    };
    let created = surface.create_group(&group, "op-create").await.unwrap();
    assert_eq!(created.not_added, vec!["u_stranger".to_owned()]);
    assert_eq!(
        served.last(),
        (
            "create_group".into(),
            json!({"group":{"name":"refund · agents","invite":["u_alice","u_stranger"]},"operation_id":"op-create"})
        )
    );

    surface
        .invite(&created.group_id, &["u_bob".into()], "op-invite")
        .await
        .unwrap();
    assert_eq!(
        served.last(),
        (
            "invite".into(),
            json!({"group_id":created.group_id,"users":["u_bob"],"operation_id":"op-invite"})
        )
    );

    surface
        .rename_group(&created.group_id, "refund · archived", "op-rename")
        .await
        .unwrap();
    assert_eq!(
        served.last(),
        (
            "rename_group".into(),
            json!({"group_id":created.group_id,"name":"refund · archived","operation_id":"op-rename"})
        )
    );

    let adopted = surface.adopt_group("om_bind").await.unwrap();
    assert_eq!(adopted.group_id, "oc_bound");
    assert_eq!(
        served.last(),
        ("adopt_group".into(), json!({"message_ref":"om_bind"}))
    );
    assert!(surface.release_group("oc_bound").await.unwrap());
    assert_eq!(
        served.last(),
        ("release_group".into(), json!({"group_id":"oc_bound"}))
    );

    // A retry with the same operation_id is the same operation.
    let again = surface.post_card(root, &card(), "op-card").await.unwrap();
    assert_eq!(again.message_ref, carded.message_ref);
    assert!(again.duplicate);
    let recreated: CreatedGroup = surface.create_group(&group, "op-create").await.unwrap();
    assert_eq!(recreated, created);

    // The adapter's error kinds reach the caller as SurfaceError kinds.
    assert!(matches!(
        surface.rename_group(&created.group_id, "", "op-empty").await,
        Err(SurfaceError::Rejected(detail)) if detail == "empty name"
    ));
    served.stop().await;
}

#[tokio::test]
async fn an_adapter_from_before_the_new_ops_keeps_working() {
    let served = serve(&["post", "adopt_group", "release_group"]).await;
    let surface = &served.surface;
    let root = PostTarget::NewRoot {
        group_id: "oc_bound".into(),
    };
    let reply = PostTarget::Reply {
        message_ref: "om_ask".into(),
        in_thread: false,
    };
    surface
        .post(root.clone(), "hello", "op-post")
        .await
        .unwrap();
    assert_eq!(
        surface.adopt_group("om_bind").await.unwrap().group_id,
        "oc_bound"
    );
    assert!(surface.release_group("oc_bound").await.unwrap());
    let before = served.calls.lock().unwrap().len();

    let unsupported = |result: Result<(), SurfaceError>| {
        assert!(matches!(result, Err(SurfaceError::Unsupported(_))));
    };
    unsupported(
        surface
            .post_mentioning(reply, "hi", "u_alice", "a")
            .await
            .map(drop),
    );
    unsupported(surface.post_card(root, &card(), "b").await.map(drop));
    unsupported(surface.update_card("om_1", &card(), "c").await);
    unsupported(surface.pin("om_1", "d").await);
    let group = NewGroup {
        name: "agents".into(),
        invite: vec![],
    };
    unsupported(surface.create_group(&group, "e").await.map(drop));
    unsupported(surface.invite("oc_bound", &["u_bob".into()], "f").await);
    unsupported(surface.rename_group("oc_bound", "archived", "g").await);
    assert_eq!(
        served.calls.lock().unwrap().len(),
        before,
        "an op the adapter did not advertise never reaches it"
    );
    served.stop().await;
}

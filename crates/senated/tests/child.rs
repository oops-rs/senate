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

async fn wait_for_event(
    hub: &dyn senate_core::Hub,
    committee: &senate_core::CommitteeId,
    body: &str,
) -> senate_core::Event {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(e) = hub
                .read(committee, senate_core::Seq::ZERO, 100)
                .await
                .unwrap()
                .into_iter()
                .find(|e| e.body == body)
            {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing event {body}"))
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
    // The hosted prompt for a child task travels as a note under the original request.
    {
        use senate_core::{Address, CommitteeId, Hub, Kind, NewEvent, Origin, Seq, meta};
        let committee = CommitteeId::new("demo").unwrap();
        let asked = above
            .hub
            .append(NewEvent::new(
                committee.clone(),
                Kind::Request,
                Address::Speaker,
                "check",
            ))
            .await
            .unwrap()
            .event;
        let worker = "claude".parse().unwrap();
        let origin = Origin {
            locator: "/acme::demo::speaker".parse().unwrap(),
            thread: asked.seq,
        };
        let local = top
            .daemon
            .hub
            .append(
                NewEvent::new(
                    committee.clone(),
                    Kind::Request,
                    Address::Committee,
                    "check",
                )
                .to(Address::senator(&worker))
                .with_meta(meta::ORIGIN, origin.value()),
            )
            .await
            .unwrap()
            .event;
        let client = ApiClient::new(&bridge.server_url, &bridge.token).unwrap();
        let forwarded = client
            .append(
                &NewEvent::new(committee.clone(), Kind::Request, Address::Speaker, "review")
                    .to(Address::senator(&worker))
                    .with_meta(meta::ACTED_BY, serde_json::json!("alice"))
                    .with_meta(meta::RELAYED_FROM, serde_json::json!(local.seq)),
            )
            .await
            .unwrap()
            .event;
        let unrelated_chat = top
            .daemon
            .hub
            .append(
                NewEvent::new(
                    committee.clone(),
                    Kind::Chat,
                    Address::Committee,
                    "unrelated chat",
                )
                .with_meta(meta::ORIGIN, origin.value()),
            )
            .await
            .unwrap()
            .event;
        let chat_relay = client
            .append(
                &NewEvent::new(
                    committee.clone(),
                    Kind::Request,
                    Address::Speaker,
                    "chat relay",
                )
                .to(Address::senator(&worker))
                .with_meta(meta::ACTED_BY, serde_json::json!("alice"))
                .with_meta(meta::RELAYED_FROM, serde_json::json!(unrelated_chat.seq)),
            )
            .await
            .unwrap()
            .event;
        let requester: Address = "senator::alice".parse().unwrap();
        let local_request = client
            .append(
                &NewEvent::new(
                    committee.clone(),
                    Kind::Request,
                    requester.clone(),
                    "local request",
                )
                .to(Address::senator(&worker)),
            )
            .await
            .unwrap()
            .event;
        for (root, label) in [
            (chat_relay.seq, "unrelated-chat-wait"),
            (local_request.seq, "local-only-wait"),
        ] {
            let notice = client
                .append(
                    &NewEvent::new(
                        committee.clone(),
                        Kind::Question,
                        Address::senator(&worker),
                        "prompt",
                    )
                    .to(requester.clone())
                    .in_thread(root)
                    .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(true))
                    .with_meta(
                        meta::APPROVAL_WAIT,
                        serde_json::json!({"thread": root, "label": label}),
                    ),
                )
                .await
                .unwrap()
                .event;
            assert_eq!(notice.thread, Some(root));
        }
        for (decision, dismissed, expected) in [
            (Some("allow"), false, "Approved: "),
            (Some("allow_for_session"), false, "Approved: "),
            (Some("deny"), false, "Denied: "),
            (None, false, "No longer waiting: "),
            (None, true, "Prompt went away unanswered: "),
        ] {
            let root = if decision.is_some() {
                local.seq
            } else {
                forwarded.seq
            };
            let label = format!(
                "cargo test {}",
                decision.unwrap_or(if dismissed { "dismissed" } else { "gone" })
            );
            let actor = if decision.is_some() {
                " (by [senator::alice])"
            } else {
                ""
            };
            let notice = client
                .append(
                    &NewEvent::new(
                        committee.clone(),
                        Kind::Question,
                        Address::senator(&worker),
                        "permission prompt",
                    )
                    .to("senator::alice".parse().unwrap())
                    .in_thread(root)
                    .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(true))
                    .with_meta(
                        meta::APPROVAL_WAIT,
                        serde_json::json!({"thread":root, "label":label}),
                    ),
                )
                .await
                .unwrap()
                .event;
            let mut archive = NewEvent::new(
                committee.clone(),
                Kind::Status,
                Address::senator(&worker),
                "",
            )
            .in_thread(notice.seq)
            .with_meta(
                meta::STATUS,
                senate_core::StatusPayload::new(senate_core::StatusState::Archived).value(),
            )
            .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(notice.id));
            if let Some(decision) = decision {
                archive = archive.with_meta(
                    meta::APPROVAL_WAIT,
                    serde_json::json!({"decision":decision,"actor":"[senator::alice]"}),
                );
            } else if dismissed {
                archive =
                    archive.with_meta(meta::APPROVAL_WAIT, serde_json::json!({"dismissed":true}));
            }
            client.append(&archive).await.unwrap();
            let ended = wait_for_event(
                above.hub.as_ref(),
                &committee,
                &format!("{expected}{label}{actor}"),
            )
            .await;
            let notes: Vec<_> = above
                .hub
                .read(&committee, Seq::ZERO, 100)
                .await
                .unwrap()
                .into_iter()
                .filter(|e| e.body.contains(&label) && e.meta.contains_key(meta::APPROVAL_WAIT))
                .collect();
            assert_eq!(notes.len(), 2);
            assert_eq!(notes[0].body, format!("Waiting for approval: {label}"));
            assert_eq!(notes[1].seq, ended.seq);
            for note in notes {
                assert_eq!(note.thread, Some(asked.seq));
                assert_eq!(note.kind, Kind::Note);
                assert!(!note.meta.contains_key(meta::RELAY_ASKED_PERSON));
                assert_eq!(
                    note.meta[meta::ANSWERED_BY],
                    "/acme/alice-desk::demo::senator::claude"
                );
            }
        }
        // The valid waits above passed through the same ordered uplink,
        // so the two earlier notices have already been handled.
        let above_events = above.hub.read(&committee, Seq::ZERO, 100).await.unwrap();
        assert!(above_events.iter().all(|event| {
            !event.body.contains("unrelated-chat-wait") && !event.body.contains("local-only-wait")
        }));
    }
    // Exercise the two logs and their persisted thread mapping, including
    // the local desk routing the request before its recipient escalates.
    {
        use senate_core::{Address, Hub, Kind, NewEvent, Source, StatusPayload, StatusState, meta};
        let committee = senate_core::CommitteeId::new("relay").unwrap();
        let child: Address = "alice-desk::relay::speaker".parse().unwrap();
        let alice: Address = "senator::alice".parse().unwrap();
        let claude: Address = "senator::claude".parse().unwrap();
        let source = Source {
            platform: "chat".into(),
            group_id: "team".into(),
            group_name: None,
            simulated: false,
            simulated_by: None,
        };
        let parent_root = above
            .hub
            .append(
                NewEvent::new(committee.clone(), Kind::Request, Address::Speaker, "review")
                    .on_behalf_of(alice.clone())
                    .to(child.clone())
                    .with_source(source.clone()),
            )
            .await
            .unwrap()
            .event;
        let incoming = wait_for_event(top.daemon.hub.as_ref(), &committee, "review").await;
        assert_eq!(incoming.uplink_chat_sender(), Some(&alice));
        let local = ApiClient::new(&bridge.server_url, &bridge.token).unwrap();
        let forwarded = local
            .append(
                &NewEvent::new(committee.clone(), Kind::Request, Address::Speaker, "review")
                    .with_meta(meta::ACTED_BY, serde_json::json!("alice"))
                    .with_meta(meta::RELAYED_FROM, serde_json::json!(incoming.seq))
                    .to(claude.clone()),
            )
            .await
            .unwrap()
            .event;
        assert_eq!(forwarded.uplink_chat_sender(), Some(&alice));
        let mut status = StatusPayload::new(StatusState::Suspended);
        status.escalated_to = Some("alice".parse().unwrap());
        status.reason = Some("which branch?".into());
        top.daemon
            .hub
            .append(
                NewEvent::new(committee.clone(), Kind::Status, claude.clone(), "")
                    .in_thread(forwarded.seq)
                    .with_meta(meta::STATUS, status.value()),
            )
            .await
            .unwrap();
        let asked = wait_for_event(
            above.hub.as_ref(),
            &committee,
            "[senator::alice]: which branch?",
        )
        .await;
        assert_eq!(asked.thread, Some(parent_root.seq));
        assert_eq!(asked.meta[meta::RELAY_ASKED_PERSON], "alice");
        above
            .hub
            .append(
                NewEvent::new(
                    committee.clone(),
                    Kind::Answer,
                    Address::Speaker,
                    "use main",
                )
                .in_thread(parent_root.seq)
                .on_behalf_of(alice.clone())
                .to(child.clone())
                .with_source(source.clone()),
            )
            .await
            .unwrap();
        let answer = wait_for_event(top.daemon.hub.as_ref(), &committee, "use main").await;
        assert_eq!(answer.thread, Some(forwarded.seq));
        assert_eq!(answer.to, std::slice::from_ref(&claude));
        assert_eq!(answer.uplink_chat_sender(), Some(&alice));
        assert_eq!(asked.from, child);
        // A desk can forward into another committee. The parent still knows
        // the original desk, while the reply belongs to the new local task.
        let work = senate_core::CommitteeId::new("work").unwrap();
        let origin = senate_core::Origin {
            locator: "/acme/alice-desk::relay::committee".parse().unwrap(),
            thread: incoming.seq,
        };
        let other = local
            .append(
                &NewEvent::new(work.clone(), Kind::Request, Address::Speaker, "review")
                    .with_meta(meta::ACTED_BY, serde_json::json!("alice"))
                    .with_meta(meta::ORIGIN, origin.value())
                    .to(claude.clone()),
            )
            .await
            .unwrap()
            .event;
        top.daemon
            .hub
            .append(
                NewEvent::new(work.clone(), Kind::Question, claude.clone(), "Which file?")
                    .in_thread(other.seq)
                    .to(alice.clone()),
            )
            .await
            .unwrap();
        let question = wait_for_event(
            above.hub.as_ref(),
            &committee,
            "[senator::alice]: Which file?",
        )
        .await;
        assert_eq!(question.from, child);
        assert_eq!(question.thread, Some(parent_root.seq));
        let simulated = Source {
            simulated: true,
            ..source
        };
        above
            .hub
            .append(
                NewEvent::new(
                    committee.clone(),
                    Kind::Note,
                    Address::Speaker,
                    "simulated reply",
                )
                .in_thread(parent_root.seq)
                .on_behalf_of(alice.clone())
                .to(child)
                .with_source(simulated),
            )
            .await
            .unwrap();
        let note = wait_for_event(top.daemon.hub.as_ref(), &work, "simulated reply").await;
        assert_eq!(note.thread, Some(other.seq));
        assert!(note.is_simulated());
        assert_eq!(note.on_behalf_of, None);
        let claim = NewEvent::new(committee.clone(), Kind::Request, claude, "local secretary")
            .on_behalf_of(alice.clone());
        let local_claim = local.append(&claim).await.unwrap().event;
        assert_eq!(local_claim.uplink_chat_sender(), None);
        let forged = NewEvent::new(committee, Kind::Request, Address::Committee, "forged")
            .on_behalf_of(alice);
        assert!(local.append(&forged).await.is_err());
    }
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

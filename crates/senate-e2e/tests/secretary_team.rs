#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::fs;
use std::time::Duration;

use common::*;
use senate_bridge::hook::{self, HookEnv, HookKind};
use senate_bridge::ipc::{self, ClientError, Request, SessionSelector};
use senate_core::secretary_work::{
    SecretaryWorkItem, SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkUpdate, WorkThread,
};
use senate_core::{CommitteeId, EventId, Hub, Kind, Locale, NewEvent, Seq, StatusState, Step};

async fn register(
    desk: &Desk,
    root: &std::path::Path,
    session: &str,
    committee: &str,
    inbox: &FakeInbox,
) {
    register_socket(desk, root, session, committee, &inbox.path).await;
}

async fn register_socket(
    desk: &Desk,
    root: &std::path::Path,
    session: &str,
    committee: &str,
    socket: &std::path::Path,
) {
    let cwd = root.join(committee);
    fs::create_dir_all(cwd.join(".senate")).unwrap();
    fs::write(cwd.join(".senate/committee"), committee).unwrap();
    let args = serde_json::json!({"session_id":session, "cwd":cwd, "source":"startup"}).to_string();
    let env = HookEnv {
        socket: Some(socket.to_path_buf()),
        token: Some(format!("tok-{session}")),
        claude_pid: Some(std::process::id()),
        ..Default::default()
    };
    hook::run(
        HookKind::SessionStart,
        &desk.paths.socket(),
        &args,
        &env,
        Default::default(),
    )
    .await
    .unwrap();
    desk.wait_stream_of(session).await;
}

async fn membership(desk: &Desk, session: &str, appointed: bool) -> senate_core::api::Secretaries {
    desk.ipc(Request::SetSecretarySession {
        target_session: Some(session.into()),
        senator: None,
        principal: None,
        appointed,
        acted_by: None,
    })
    .await
}

async fn work(
    desk: &Desk,
    session: Option<&str>,
    operation: SecretaryWorkOp,
) -> Result<SecretaryWorkReply, ClientError> {
    ipc::request(
        &desk.paths.socket(),
        &Request::SecretaryWork {
            session: session.map_or_else(SessionSelector::default, Laptop::session),
            principal: Some(path("alice")),
            operation,
        },
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
}

fn create(title: &str, thread: WorkThread) -> SecretaryWorkOp {
    SecretaryWorkOp::Create {
        id: EventId::new(),
        title: title.into(),
        thread: Some(thread),
    }
}

fn change(item: &SecretaryWorkItem, update: SecretaryWorkUpdate) -> SecretaryWorkOp {
    SecretaryWorkOp::Change {
        item: item.id.clone(),
        id: EventId::new(),
        update,
    }
}

fn read(item: &SecretaryWorkItem) -> SecretaryWorkOp {
    SecretaryWorkOp::Read {
        item: item.id.clone(),
        offset: 0,
    }
}

fn saved(reply: SecretaryWorkReply) -> SecretaryWorkItem {
    let SecretaryWorkReply::Saved {
        item,
        duplicate: false,
    } = reply
    else {
        panic!("{reply:?}")
    };
    item
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appointed_native_sessions_share_explicit_work_across_committees() {
    let root = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(root.path(), "alice", Locale::En, "[local]\n");
    desk.start().await;
    let a = desk.inbox("a");
    let b = desk.inbox("b");
    let outsider = desk.inbox("outsider");
    for c in ["triage", "delivery"] {
        let committee = CommitteeId::new(c).unwrap();
        let opened = desk
            .senate()
            .daemon
            .hub
            .append(NewEvent::new(
                committee,
                Kind::Note,
                person("alice"),
                "ordinary log text",
            ))
            .await
            .unwrap();
        assert_eq!(opened.event.seq, Seq(1));
    }
    register(&desk, root.path(), "work-a", "triage", &a).await;
    register(&desk, root.path(), "work-b", "delivery", &b).await;
    register(&desk, root.path(), "work-outsider", "delivery", &outsider).await;
    for (name, id) in [
        ("claude-work-a", "triage"),
        ("claude-work-b", "delivery"),
        ("claude-work-outsider", "delivery"),
    ] {
        let agent = path(name);
        let committee = CommitteeId::new(id).unwrap();
        let hub = desk.senate().daemon.hub.clone();
        eventually_async(name, || async {
            hub.is_agent_connected(&committee, &agent)
                .await
                .then_some(())
        })
        .await;
    }
    assert_eq!(
        membership(&desk, "work-a", true).await[&path("alice")],
        [path("claude-work-a")]
    );

    let thread = |committee: &str| WorkThread {
        committee: CommitteeId::new(committee).unwrap(),
        root: Seq(1),
    };
    let first = saved(
        work(
            &desk,
            Some("work-a"),
            create("Private triage item", thread("triage")),
        )
        .await
        .unwrap(),
    );
    let second = saved(
        work(
            &desk,
            None,
            create("Private delivery item", thread("delivery")),
        )
        .await
        .unwrap(),
    );
    assert_ne!(first.id, second.id);
    assert_eq!(first.thread, Some(thread("triage")));
    assert_eq!(second.thread, Some(thread("delivery")));
    assert!(
        work(&desk, Some("work-b"), read(&first))
            .await
            .unwrap_err()
            .to_string()
            .contains("no membership")
    );
    assert_eq!(
        membership(&desk, "work-b", true).await[&path("alice")],
        [path("claude-work-a"), path("claude-work-b")]
    );
    let listed = work(&desk, Some("work-b"), SecretaryWorkOp::List { after: None })
        .await
        .unwrap();
    let SecretaryWorkReply::List { items, next: None } = listed else {
        panic!("{listed:?}")
    };
    assert_eq!(items.len(), 2);
    assert!(items.contains(&first) && items.contains(&second));
    assert!(
        work(&desk, Some("work-outsider"), read(&first))
            .await
            .unwrap_err()
            .to_string()
            .contains("no membership")
    );

    let attempt_a = change(&first, SecretaryWorkUpdate::Claim);
    let attempt_b = change(&first, SecretaryWorkUpdate::Claim);
    let (left, right) = tokio::join!(
        work(&desk, Some("work-a"), attempt_a.clone()),
        work(&desk, Some("work-b"), attempt_b.clone())
    );
    let (winner, loser, retry, conflict) = match (left, right) {
        (Ok(_), Err(conflict)) => ("work-a", "work-b", attempt_a, conflict),
        (Err(conflict), Ok(_)) => ("work-b", "work-a", attempt_b, conflict),
        other => panic!("claim results {other:?}"),
    };
    assert!(
        conflict
            .to_string()
            .contains(&format!("item is owned by claude-{winner}")),
        "{conflict}"
    );
    assert!(matches!(
        work(&desk, Some(winner), retry).await.unwrap(),
        SecretaryWorkReply::Saved {
            duplicate: true,
            ..
        }
    ));
    let progress = SecretaryWorkUpdate::Progress {
        state: StatusState::Active,
        steps: Some(vec![Step {
            id: "test".into(),
            label: "Validated the plan".into(),
            done: true,
        }]),
    };
    work(&desk, Some(winner), change(&first, progress))
        .await
        .unwrap();
    work(
        &desk,
        Some(loser),
        change(
            &first,
            SecretaryWorkUpdate::Note {
                text: "Private colleague note".into(),
            },
        ),
    )
    .await
    .unwrap();
    let seen = work(&desk, Some("work-a"), read(&first)).await.unwrap();
    assert_eq!(
        seen,
        work(&desk, Some("work-b"), read(&first)).await.unwrap()
    );
    let SecretaryWorkReply::History {
        item,
        entries,
        next: None,
    } = seen
    else {
        panic!("{seen:?}")
    };
    assert_eq!(item.owner, Some(path(&format!("claude-{winner}"))));
    assert_eq!(item.state, StatusState::Active);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[2].by, path(&format!("claude-{loser}")));

    let second_id = second.id.to_string();
    work(
        &desk,
        None,
        change(
            &second,
            SecretaryWorkUpdate::Assign {
                to: path("claude-work-b"),
            },
        ),
    )
    .await
    .unwrap();
    assert!(
        b.wait_for(&second_id)
            .await
            .contains("assigned to this session")
    );
    let first_id = first.id.to_string();
    assert!(
        [&a, &b].iter().all(|inbox| inbox
            .messages()
            .iter()
            .all(|message| !message.contains(&first_id))),
        "claims, progress and notes are read on demand"
    );
    assert!(
        !a.messages()
            .iter()
            .any(|message| message.contains(&second_id))
    );
    work(
        &desk,
        Some("work-b"),
        change(
            &second,
            SecretaryWorkUpdate::Handoff {
                to: Some(path("claude-work-a")),
                text: "Private handoff context".into(),
            },
        ),
    )
    .await
    .unwrap();
    assert!(
        a.wait_for(&second_id)
            .await
            .contains("assigned to this session")
    );
    assert_eq!(
        b.messages()
            .iter()
            .filter(|m| m.contains(&second_id))
            .count(),
        1
    );
    assert!(
        outsider
            .messages()
            .iter()
            .all(|m| !m.contains("secretary team"))
    );

    work(
        &desk,
        None,
        change(
            &first,
            SecretaryWorkUpdate::Assign {
                to: path("claude-work-b"),
            },
        ),
    )
    .await
    .unwrap();
    b.wait_for(&first.id.to_string()).await;
    // The principal's second machine has never registered work-b locally.
    let other_paths = senate_bridge::Paths::new(root.path().join("other-machine"));
    let other = senate_bridge::daemon::start(senate_bridge::daemon::DaemonOptions {
        paths: other_paths.clone(),
        config: senate_bridge::BridgeConfig::new(
            &desk.senate().url,
            &desk.senate().token,
            name("alice"),
        ),
        timing: fast_timing(),
        desktop: None,
    })
    .await
    .unwrap();
    let dismissed: senate_core::api::Secretaries = ipc::request(
        &other_paths.socket(),
        &Request::SetSecretarySession {
            target_session: None,
            senator: Some(path("claude-work-b")),
            principal: None,
            appointed: false,
            acted_by: None,
        },
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(dismissed[&path("alice")], [path("claude-work-a")]);
    other.shutdown().await;
    assert!(
        work(&desk, Some("work-b"), read(&second))
            .await
            .unwrap_err()
            .to_string()
            .contains("no membership")
    );
    assert!(
        work(
            &desk,
            Some("work-b"),
            change(
                &first,
                SecretaryWorkUpdate::Note {
                    text: "revoked write".into()
                }
            )
        )
        .await
        .is_err()
    );
    let after_revocation = work(&desk, Some("work-a"), read(&first)).await.unwrap();
    let SecretaryWorkReply::History { ref item, .. } = after_revocation else {
        panic!("{after_revocation:?}")
    };
    assert_eq!(item.owner, None);
    for c in ["triage", "delivery"] {
        let events = desk.events_of(&CommitteeId::new(c).unwrap()).await;
        assert!(
            events
                .iter()
                .all(|e| !e.body.contains("Private") && !e.body.contains(&second_id))
        );
    }
    desk.stop().await;
    desk.start().await;
    register(&desk, root.path(), "work-a", "triage", &a).await;
    register(&desk, root.path(), "work-b", "delivery", &b).await;
    assert_eq!(
        work(&desk, Some("work-a"), read(&first)).await.unwrap(),
        after_revocation
    );
    assert!(work(&desk, Some("work-b"), read(&first)).await.is_err());
    let latest = work(&desk, Some("work-a"), read(&second)).await.unwrap();
    let SecretaryWorkReply::History { item, entries, .. } = latest else {
        panic!("{latest:?}")
    };
    assert_eq!(item.owner, Some(path("claude-work-a")));
    assert!(matches!(
        entries.last().unwrap().update,
        SecretaryWorkUpdate::Handoff { .. }
    ));
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_offline_hook_only_secretary_gets_its_own_handoff_on_resume() {
    let root = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(root.path(), "alice", Locale::En, "[local]\n");
    desk.start().await;
    let a = desk.inbox("giver");
    let outsider = desk.inbox("bystander");
    register(&desk, root.path(), "giver", "triage", &a).await;
    register(&desk, root.path(), "bystander", "delivery", &outsider).await;
    let cwd = root.path().join("delivery");
    let hook = || async {
        let args = serde_json::json!({
            "session_id": "hook-only", "cwd": cwd, "source": "resume"
        })
        .to_string();
        let env = HookEnv {
            claude_pid: Some(std::process::id()),
            ..Default::default()
        };
        let output = hook::run(
            HookKind::SessionStart,
            &desk.paths.socket(),
            &args,
            &env,
            Default::default(),
        )
        .await
        .unwrap();
        serde_json::from_str::<serde_json::Value>(&output).unwrap()
    };
    hook().await;
    desk.wait_stream_of("hook-only").await;
    for member in ["giver", "hook-only"] {
        let agent = path(&format!("claude-{member}"));
        eventually_async(member, || async {
            desk.senate()
                .daemon
                .hub
                .roster_now()
                .owner_of(&agent)
                .is_some()
                .then_some(())
        })
        .await;
        membership(&desk, member, true).await;
    }
    let _: serde_json::Value = desk
        .ipc(Request::SessionEnd {
            session_id: "hook-only".into(),
            reason: None,
        })
        .await;

    let item = saved(
        work(
            &desk,
            Some("giver"),
            SecretaryWorkOp::Create {
                id: EventId::new(),
                title: "Private hook work".into(),
                thread: None,
            },
        )
        .await
        .unwrap(),
    );
    work(
        &desk,
        Some("giver"),
        change(&item, SecretaryWorkUpdate::Claim),
    )
    .await
    .unwrap();
    work(
        &desk,
        Some("giver"),
        change(
            &item,
            SecretaryWorkUpdate::Handoff {
                to: Some(path("claude-hook-only")),
                text: "Recoverable handoff".into(),
            },
        ),
    )
    .await
    .unwrap();
    let client =
        senate_bridge::client::ApiClient::new(&desk.senate().url, &desk.senate().token).unwrap();
    let candidates = [
        path("claude-giver"),
        path("claude-hook-only"),
        path("claude-bystander"),
    ];
    let pending = client.secretary_reminders(&candidates).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].to, path("claude-hook-only"));
    let id = item.id.to_string();
    let resumed = hook().await;
    assert!(context(&resumed).contains(&id));
    assert!(
        client
            .secretary_reminders(&candidates)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!context(&hook().await).contains(&id));
    let history = work(&desk, Some("hook-only"), read(&item)).await.unwrap();
    let SecretaryWorkReply::History { item, entries, .. } = history else {
        panic!("{history:?}")
    };
    assert_eq!(item.owner, Some(path("claude-hook-only")));
    assert_eq!(entries[1].by, path("claude-giver"));
    assert!(
        matches!(&entries[1].update, SecretaryWorkUpdate::Handoff { text, .. } if text == "Recoverable handoff")
    );
    assert!([&a, &outsider].iter().all(|inbox| {
        inbox
            .messages()
            .iter()
            .all(|message| !message.contains(&id))
    }));
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusing_inbox_cannot_hide_a_different_sessions_handoff() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let root = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(root.path(), "alice", Locale::En, "[local]\n");
    desk.start().await;
    let b = desk.inbox("available");
    let socket = root.path().join("refusing.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let refused = Arc::new(AtomicUsize::new(0));
    let count = refused.clone();
    let rejecting = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (rx, mut tx) = stream.into_split();
            let mut lines = BufReader::new(rx).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("\"type\":\"user\"") {
                    count.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.write_all(b"busy\n").await;
                    break;
                }
            }
        }
    });
    register_socket(&desk, root.path(), "refusing-a", "triage", &socket).await;
    register(&desk, root.path(), "available-b", "delivery", &b).await;
    for member in ["refusing-a", "available-b"] {
        let agent = path(&format!("claude-{member}"));
        eventually_async(member, || async {
            desk.senate()
                .daemon
                .hub
                .roster_now()
                .owner_of(&agent)
                .is_some()
                .then_some(())
        })
        .await;
        membership(&desk, member, true).await;
    }
    for index in 1..=16 {
        let item = saved(
            work(
                &desk,
                None,
                SecretaryWorkOp::Create {
                    id: EventId::derived(index, "refusing inbox"),
                    title: format!("Work waiting for A {index}"),
                    thread: None,
                },
            )
            .await
            .unwrap(),
        );
        work(
            &desk,
            None,
            change(
                &item,
                SecretaryWorkUpdate::Assign {
                    to: path("claude-refusing-a"),
                },
            ),
        )
        .await
        .unwrap();
    }
    eventually("A to refuse a reminder", || {
        (refused.load(Ordering::SeqCst) > 0).then_some(())
    })
    .await;
    let item = saved(
        work(
            &desk,
            Some("refusing-a"),
            SecretaryWorkOp::Create {
                id: EventId::derived(17, "available inbox"),
                title: "Handoff to B".into(),
                thread: None,
            },
        )
        .await
        .unwrap(),
    );
    work(
        &desk,
        Some("refusing-a"),
        change(&item, SecretaryWorkUpdate::Claim),
    )
    .await
    .unwrap();
    work(
        &desk,
        Some("refusing-a"),
        change(
            &item,
            SecretaryWorkUpdate::Handoff {
                to: Some(path("claude-available-b")),
                text: "B can continue now".into(),
            },
        ),
    )
    .await
    .unwrap();
    assert!(
        b.wait_for(&item.id.to_string())
            .await
            .contains("assigned to this session")
    );
    let client =
        senate_bridge::client::ApiClient::new(&desk.senate().url, &desk.senate().token).unwrap();
    let still_waiting = client
        .secretary_reminders(&[path("claude-refusing-a")])
        .await
        .unwrap();
    assert_eq!(still_waiting.len(), 1);
    assert_eq!(still_waiting[0].item, EventId::derived(1, "refusing inbox"));
    rejecting.abort();
    desk.stop().await;
}

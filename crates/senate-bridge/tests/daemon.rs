//! The daemon against a fake server and fake inbox sockets: streams,
//! cursors, routing, batching, the hook queue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::time::Duration;

use common::fake_codex::input_text;
use common::*;
use senate_bridge::SessionState;
use senate_bridge::daemon::{MAX_BATCH_EVENTS, PENDING_CAP};
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, IpcClient, Request, StatusReply, StreamState, WatchEvent};
use senate_bridge::state::ItemStatus;
use senate_core::{Address, Kind, NewEvent, Refs, Seq};
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directed_question_is_pushed_once_with_the_inbox_framing() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    let start = h
        .hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await
        .expect("a bound session gets orientation at SessionStart");
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(context(&start).contains(
        "joined to the committee `refund-v2` of the Senate [/senate] as [senator::claude]"
    ));
    h.wait_stream("refund-v2").await;

    h.server.append(relayed_question("退款上限是多少天？"));
    let conns = eventually("one inbox connection", WAIT, || {
        let c = inbox.connections();
        (!c.is_empty()).then_some(c)
    })
    .await;
    let lines = &conns[0];
    assert_eq!(
        lines.len(),
        2,
        "auth line, user line, then close: {lines:?}"
    );
    assert_eq!(lines[0], r#"{"type":"auth","token":"tok-s1"}"#);
    let user: Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(user["type"], "user");
    assert_eq!(user["message"]["role"], "user");
    let content = user["message"]["content"].as_str().unwrap();
    assert!(content.starts_with(
        "[senate] The messages below come from the Senate committee log for refund-v2"
    ));
    assert!(content.contains("None was written by your user"));
    assert!(content.contains(
        "[senate · refund-v2 · seq 1 · from [speaker::pm] · question]\n\
         │ 退款上限是多少天？\n\
         ↳ reply with mcp__senate__answer(thread=1) — it is posted back to [senator::pm]"
    ));

    h.wait_delivered_cursor("refund-v2", 1).await;
    eventually("a heartbeat naming one session", WAIT, || {
        h.server
            .heartbeats()
            .iter()
            .any(|(f, who, hb)| {
                f == "refund-v2"
                    && who == "alice"
                    && hb.agent == path("claude")
                    && hb.sessions == 1
                    && hb.cursor == Seq(1)
            })
            .then_some(())
    })
    .await;
    // The stream was claude's own connection.
    assert!(
        h.server
            .stream_opens()
            .iter()
            .all(|o| o.person == "alice" && o.senator.as_deref() == Some("claude"))
    );
    eventually(
        "the inbox delivery and socket status are both recorded",
        WAIT,
        || {
            let item = h.item(1)?;
            let session = h.store().session("s1").unwrap()?;
            (item.status == ItemStatus::Delivered && session.socket_ok == Some(true)).then_some(())
        },
    )
    .await;

    let marker = h
        .server
        .append(note(person("pm"), "Inbox delivery barrier.").to(claude("alice")));
    eventually("a later inbox push was acknowledged", WAIT, || {
        (h.item(marker.seq.0)?.status == ItemStatus::Delivered).then_some(())
    })
    .await;
    let messages = inbox.messages();
    assert!(
        messages
            .iter()
            .any(|text| pos(text, marker.seq.0).is_some()),
        "{messages:?}"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|text| pos(text, 1).is_some())
            .count(),
        1
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_becomes_one_inbox_message_in_seq_order() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;

    h.server.append(relayed_question("first"));
    h.server
        .append(question(claude("bob"), claude("alice"), "second"));
    h.server.append(note(person("pm"), "undirected chatter"));
    h.server.append(
        NewEvent::new(committee(), Kind::Note, Address::Speaker, "fourth").to(claude("alice")),
    );

    let msgs = inbox.wait_messages(1).await;
    eventually("the whole directed burst was acknowledged", WAIT, || {
        [1, 2, 4]
            .into_iter()
            .all(|seq| {
                h.item(seq)
                    .is_some_and(|item| item.status == ItemStatus::Delivered)
            })
            .then_some(())
    })
    .await;
    assert_eq!(inbox.messages().len(), 1, "one message for the whole burst");
    let m = &msgs[0];
    let (p1, p2, p4) = (pos(m, 1).unwrap(), pos(m, 2).unwrap(), pos(m, 4).unwrap());
    assert!(p1 < p2 && p2 < p4, "seq order: {m}");
    assert!(pos(m, 3).is_none(), "undirected events wait for catch_up");
    assert_eq!(m.matches("[senate] The messages below").count(), 1);
    h.stop().await;
}

/// Criterion 3, live: the stream drops, traffic happens, the stream comes
/// back with `Last-Event-ID` = the delivered cursor and the directed backlog
/// arrives as one message in seq order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_resumes_from_the_delivered_cursor_in_order() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("before the gap"));
    inbox.wait_messages(1).await;
    h.wait_delivered_cursor("refund-v2", 1).await;

    h.server.set_offline(true);
    eventually("the stream to drop", WAIT, || {
        (h.server.active_streams("refund-v2") == 0).then_some(())
    })
    .await;
    let offline_mark = h.server.stream_opens().len();
    h.server.append(note(person("pm"), "2: chatter")); // undirected
    let q = h.server.append(question(
        claude("bob"),
        claude("alice"),
        "3: bob asks alice",
    ));
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            claude("alice"),
            "4: alice's own answer",
        )
        .to(claude("bob"))
        .in_thread(q.seq),
    );
    // As a chat gateway records a human reply in a mirrored thread.
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Chat,
            person("pm"),
            "5: correction from a human",
        )
        .with_source(chat_source())
        .with_to(vec![claude("alice"), claude("bob")])
        .in_thread(q.seq),
    );
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            Address::Speaker,
            "6: speaker for alice",
        )
        .on_behalf_of(claude("alice"))
        .to(claude("bob"))
        .in_thread(q.seq),
    );
    h.server.set_offline(false);

    let msgs = inbox.wait_messages(2).await;
    let m = &msgs[1];
    let (p3, p5) = (pos(m, 3).unwrap(), pos(m, 5).unwrap());
    assert!(p3 < p5, "backlog in seq order: {m}");
    for not_directed in [2, 4, 6] {
        assert!(
            pos(m, not_directed).is_none(),
            "seq {not_directed} must not be pushed: {m}"
        );
    }
    assert!(m.contains(
        "· from [senator::pm] in chat group「退款 v2 · 后端群」 · human reply in thread 3]"
    ));
    let reopened = h.server.stream_opens()[offline_mark..]
        .iter()
        .find(|o| o.accepted)
        .cloned()
        .expect("an accepted reconnect");
    assert_eq!(reopened.last_event_id.as_deref(), Some("1"));
    assert_eq!(reopened.after, 1);
    h.wait_delivered_cursor("refund-v2", 6).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(inbox.messages().len(), 2);
    h.stop().await;
}

/// Criterion 3, laptop closed: the daemon itself stops, traffic happens, the
/// restarted daemon resumes from the cursor persisted in state.db.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resumes_from_the_persisted_cursor() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("one"));
    inbox.wait_messages(1).await;
    h.wait_delivered_cursor("refund-v2", 1).await;
    h.stop().await;
    assert!(!h.socket().exists(), "shutdown removes the socket");

    h.server
        .append(question(claude("bob"), claude("alice"), "two"));
    h.server.append(note(person("pm"), "three"));
    h.server.append(relayed_question("four"));
    // A laptop with its own cursor resumes from it, whatever the server holds.
    h.server
        .set_cursor("refund-v2", "claude", "delivered", Seq(3));
    let before = h.server.stream_opens().len();

    h.start().await;
    let msgs = inbox.wait_messages(2).await;
    let m = &msgs[1];
    assert!(pos(m, 2).unwrap() < pos(m, 4).unwrap());
    assert!(pos(m, 1).is_none() && pos(m, 3).is_none());
    let first = h.server.stream_opens()[before..]
        .iter()
        .find(|o| o.accepted)
        .cloned()
        .unwrap();
    assert_eq!(first.last_event_id.as_deref(), Some("1"));
    h.wait_delivered_cursor("refund-v2", 4).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gap_in_the_live_stream_is_read_from_the_log_in_order() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;

    h.server
        .append_silently(relayed_question("missed by the stream"));
    h.server.append(relayed_question("seen live"));
    let m = &inbox.wait_messages(1).await[0];
    assert!(pos(m, 1).unwrap() < pos(m, 2).unwrap(), "{m}");
    assert!(m.contains("missed by the stream"));
    h.wait_delivered_cursor("refund-v2", 2).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn routes_to_one_session_by_refs_path_then_recency() {
    let mut h = Harness::new().await;
    h.start().await;
    let (inbox_a, inbox_b) = (h.inbox("a"), h.inbox("b"));
    let (wd_a, wd_b) = (
        h.workdir("a", Some("refund-v2")),
        h.workdir("b", Some("refund-v2")),
    );
    std::fs::create_dir_all(wd_b.join("src")).unwrap();
    std::fs::write(wd_b.join("src/idem.rs"), "// idempotency\n").unwrap();
    // B first, then A: A is the most recently active.
    h.hook(HookKind::SessionStart, "b", &wd_b, Some(&inbox_b))
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.hook(HookKind::SessionStart, "a", &wd_a, Some(&inbox_a))
        .await;
    h.wait_stream("refund-v2").await;

    // refs.path beats recency.
    h.server
        .append(relayed_question("about idem.rs").with_refs(Refs {
            path: Some("src/idem.rs".into()),
            ..Refs::default()
        }));
    let m = inbox_b.wait_messages(1).await;
    assert!(m[0].contains("about idem.rs"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        inbox_a.messages().is_empty(),
        "delivered to one session only"
    );

    // No path: the most recently active session.
    h.server.append(relayed_question("no path"));
    let m = inbox_a.wait_messages(1).await;
    assert!(m[0].contains("no path"));

    // Activity in B makes it the most recent.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        h.hook(HookKind::UserPromptSubmit, "b", &wd_b, Some(&inbox_b))
            .await
            .is_none()
    );
    h.server.append(relayed_question("after b typed"));
    let m = inbox_b.wait_messages(2).await;
    assert!(m[1].contains("after b typed"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(inbox_a.messages().len(), 1);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbound_sessions_and_own_or_undirected_events_get_nothing() {
    let mut h = Harness::new().await;
    h.start().await;
    let (inbox_u, inbox_s) = (h.inbox("u"), h.inbox("s"));
    let wd_u = h.workdir("u", None);
    let wd_s = h.workdir("s", Some("refund-v2"));
    assert!(
        h.hook(HookKind::SessionStart, "u", &wd_u, Some(&inbox_u))
            .await
            .is_none(),
        "an unbound session gets no context"
    );
    h.hook(HookKind::SessionStart, "s", &wd_s, Some(&inbox_s))
        .await;
    h.wait_stream("refund-v2").await;

    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Note,
            claude("alice"),
            "1: my own note to myself",
        )
        .with_to(vec![claude("alice"), claude("bob")]),
    );
    h.server.append(note(person("pm"), "2: undirected"));
    h.server
        .append(question(claude("alice"), claude("bob"), "3: to bob"));
    h.server
        .append(note(Address::Speaker, "4: everyone").to(Address::Committee));
    h.server
        .append(note(Address::Speaker, "5: the person").to(person("alice")));
    h.server.append(relayed_question("6: for my agent"));
    // To alice's other agent: not this session's.
    h.server
        .append(note(Address::Speaker, "7: for codex").to(codex("alice")));
    h.server.append(relayed_question("8: for my agent again"));

    let m = eventually("6 and 8 pushed", WAIT, || {
        let text = inbox_s.messages().join("\n");
        (pos(&text, 6).is_some() && pos(&text, 8).is_some()).then_some(text)
    })
    .await;
    for seq in [1, 2, 3, 4, 5, 7] {
        assert!(pos(&m, seq).is_none(), "seq {seq} must not be pushed");
    }
    // The socket can receive a push before the daemon records it as delivered.
    eventually("both inbox deliveries recorded", WAIT, || {
        [6, 8]
            .into_iter()
            .all(|seq| {
                h.item(seq)
                    .is_some_and(|item| item.status == ItemStatus::Delivered)
            })
            .then_some(())
    })
    .await;
    let queued: Vec<u64> = h
        .store()
        .undelivered()
        .unwrap()
        .iter()
        .map(|i| i.event.seq.0)
        .collect();
    assert!(queued.is_empty(), "{queued:?}");
    assert!(inbox_u.messages().is_empty());
    assert!(
        h.hook(HookKind::UserPromptSubmit, "u", &wd_u, Some(&inbox_u))
            .await
            .is_none()
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_queue_is_returned_by_user_prompt_submit_and_session_start() {
    // Configured, so the stream stays open while no session is bound.
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    let wd = h.workdir("q", Some("refund-v2"));
    // No inbox socket: the ladder falls through to the hook queue.
    h.hook(HookKind::SessionStart, "q", &wd, None).await;
    h.wait_stream("refund-v2").await;
    h.server
        .append(relayed_question("held for your next prompt"));
    eventually("the item held for hooks", WAIT, || {
        let item = h.item(1)?;
        (item.status == ItemStatus::Queued && item.session_id.as_deref() == Some("q")).then_some(())
    })
    .await;

    let out = h
        .hook(HookKind::UserPromptSubmit, "q", &wd, None)
        .await
        .expect("queued items come back");
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let ctx = context(&out);
    assert!(ctx.starts_with("[senate] The messages below"));
    assert!(ctx.contains("[senate · refund-v2 · seq 1 · from [speaker::pm] · question]"));
    assert!(ctx.contains("held for your next prompt"));
    assert!(ctx.chars().count() <= 10_000);
    assert!(
        h.hook(HookKind::UserPromptSubmit, "q", &wd, None)
            .await
            .is_none(),
        "handed over once"
    );

    // With no session at all an event waits; the next session to bind gets it.
    h.hook(HookKind::SessionEnd, "q", &wd, None).await;
    eventually("q ended", WAIT, || {
        (h.store().session("q").unwrap()?.state == SessionState::Ended).then_some(())
    })
    .await;
    h.server.append(question(
        claude("bob"),
        claude("alice"),
        "while nobody was here",
    ));
    eventually("the item ingested", WAIT, || h.item(2)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.item(2).unwrap().status, ItemStatus::Pending);
    let out = h
        .hook(HookKind::SessionStart, "r", &wd, None)
        .await
        .unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let ctx = context(&out);
    assert!(ctx.starts_with("[senate] This session is joined"));
    assert!(ctx.contains("while nobody was here"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_inbox_marks_the_session_stale_until_its_next_hook() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("d");
    let wd = h.workdir("d", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "d", &wd, Some(&inbox)).await;
    h.wait_stream("refund-v2").await;
    let path = inbox.path.clone();
    drop(inbox);
    std::fs::remove_file(&path).unwrap();

    h.server.append(relayed_question("nobody listening"));
    eventually("d stale", WAIT, || {
        let s = h.store().session("d").unwrap()?;
        (s.state == SessionState::Stale && s.socket_ok == Some(false)).then_some(())
    })
    .await;
    assert_eq!(h.item(1).unwrap().status, ItemStatus::Pending);

    let out = h
        .hook(HookKind::UserPromptSubmit, "d", &wd, None)
        .await
        .unwrap();
    assert!(context(&out).contains("nobody listening"));
    assert_eq!(
        h.store().session("d").unwrap().unwrap().state,
        SessionState::Live
    );
    h.stop().await;
}

/// A configured committee is held open for every agent this machine has a
/// session of on record; before the first session it seats nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_follow_sessions_and_configured_committees() {
    let mut h = Harness::with_committees(&["other"]).await;
    h.start().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.server.active_streams("other"), 0, "no agent yet");
    assert_eq!(h.server.active_streams("refund-v2"), 0);

    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s", &wd, None).await;
    h.wait_stream("refund-v2").await;
    h.wait_stream_of("other", "claude").await;

    let deadline = tokio::time::Instant::now() + WAIT;
    let status = loop {
        let status: StatusReply = ipc::request(
            &h.socket(),
            &Request::Status,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let connected = status
            .committees
            .iter()
            .all(|f| matches!(f.stream, Some(StreamState::Connected { .. })));
        if connected && status.committees.len() == 2 {
            break status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "streams never connected: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let names: Vec<(String, &str)> = status
        .committees
        .iter()
        .map(|f| (f.agent.to_string(), f.committee.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("claude".to_string(), "other"),
            ("claude".to_string(), "refund-v2")
        ]
    );
    assert!(status.committees[0].configured && !status.committees[1].configured);
    assert_eq!(status.sessions.len(), 1);
    assert_eq!(
        status.sessions[0].committee().map(|f| f.as_str()),
        Some("refund-v2")
    );
    assert_eq!(status.person.as_str(), "alice");
    assert_eq!(status.senate, Some("/senate".parse().unwrap()));
    assert!(status.people.is_person(&path("alice")) && status.people.is_person(&path("bob")));
    assert!(!status.people.is_person(&path("claude")));
    let text = senate_bridge::status::render(
        &senate_bridge::status::StatusView {
            daemon: Some(status.pid),
            socket: h.socket(),
            person: Some("[senator::alice]".into()),
            senate: status.senate.clone(),
            server_url: Some(status.server_url.clone()),
            committees: status.committees.clone(),
            sessions: status.sessions.clone(),
            hosted: Default::default(),
            hosted_unbound: Default::default(),
            hosted_viewer_unavailable: Default::default(),
            dropped_hooks: None,
            queue: status.queue,
            locale: status.locale,
            speaker: status.speaker.clone(),
            roles: status.roles.clone(),
            secretary_problems: Vec::new(),
        },
        chrono::Utc::now(),
    );
    assert!(text.contains("daemon: running"));
    assert!(
        text.contains("the Senate [/senate], as [senator::alice]"),
        "{text}"
    );
    assert!(text.contains("refund-v2 (file)"));
    assert!(text.contains("[senator::claude] (harness)"), "{text}");

    // The last session leaves: its committee's stream closes, the configured one stays.
    h.hook(HookKind::SessionEnd, "s", &wd, None).await;
    eventually("the refund-v2 stream to close", WAIT, || {
        (h.server.active_streams("refund-v2") == 0).then_some(())
    })
    .await;
    assert!(h.server.active_streams("other") > 0);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_daemon_refuses_to_start() {
    let mut h = Harness::new().await;
    h.start().await;
    let second = senate_bridge::daemon::start(senate_bridge::DaemonOptions {
        paths: h.paths.clone(),
        config: h.config.clone(),
        timing: fast_timing(),
        desktop: None,
    })
    .await;
    assert!(second.is_err());
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(h.socket()).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    h.stop().await;
}

/// H1: the server's log is reset under a running bridge. The new epoch
/// makes the bridge drop its old cursor and old queue, so the new log's
/// seq 1 is delivered instead of being taken for something already seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_log_epoch_resyncs_instead_of_dropping_the_new_log() {
    let mut h = Harness::new().await;
    h.start().await;
    let server = h.server.url.clone();
    let wd = h.workdir("q", Some("refund-v2"));
    // No inbox: items are held for the session's hooks.
    h.hook(HookKind::SessionStart, "q", &wd, None).await;
    h.wait_stream("refund-v2").await;
    assert_eq!(
        h.store().epoch(&server).unwrap().as_deref(),
        Some("epoch-1")
    );
    for i in 1..=3 {
        h.server.append(relayed_question(&format!("old {i}")));
    }
    eventually("the old items held for hooks", WAIT, || {
        let held = h.store().undelivered().unwrap();
        (held.len() == 3 && held.iter().all(|i| i.status == ItemStatus::Queued)).then_some(())
    })
    .await;

    let mark = h.server.stream_opens().len();
    h.server.reset_log(Some("epoch-2"));
    let reopened = eventually("a stream on the new log", WAIT, || {
        h.server.stream_opens()[mark..]
            .iter()
            .find(|o| o.accepted)
            .cloned()
    })
    .await;
    assert_eq!(reopened.after, 0, "re-seeded at the new log's head");
    assert_eq!(
        h.store().epoch(&server).unwrap().as_deref(),
        Some("epoch-2")
    );
    assert!(
        h.store().undelivered().unwrap().is_empty(),
        "the old log's items are dropped"
    );

    h.server.append(relayed_question("new 1"));
    eventually("the new seq 1 taken in", WAIT, || {
        let i = h.item(1)?;
        (i.status == ItemStatus::Queued).then_some(())
    })
    .await;
    let out = h
        .hook(HookKind::UserPromptSubmit, "q", &wd, None)
        .await
        .expect("the new item is handed over");
    let ctx = context(&out);
    assert!(ctx.contains("new 1"), "{ctx}");
    assert!(!ctx.contains("old "), "{ctx}");

    // At start, /v1/me names the epoch: no stream opens on a stale cursor.
    h.stop().await;
    h.server.reset_log(Some("epoch-3"));
    h.server.append(note(person("pm"), "history 1"));
    h.server.append(relayed_question("history 2"));
    let mark = h.server.stream_opens().len();
    h.start().await;
    let opens: Vec<StreamOpen> = eventually("a stream opened since the restart", WAIT, || {
        let opens = h.server.stream_opens()[mark..].to_vec();
        opens.iter().any(|o| o.accepted).then_some(opens)
    })
    .await;
    assert!(opens.iter().all(|o| o.accepted), "{opens:?}");
    assert_eq!(opens[0].after, 2, "seeded at the head of the new log");
    assert_eq!(
        h.store().epoch(&server).unwrap().as_deref(),
        Some("epoch-3")
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.store().undelivered().unwrap().is_empty(),
        "history is for catch_up"
    );
    h.stop().await;
}

/// H1 seen from the server's side: same epoch, but the bridge's cursor is
/// beyond the log. 409 `cursor_ahead` resyncs the committee.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_ahead_resyncs_the_committee() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("one"));
    h.server.append(relayed_question("two"));
    inbox.wait_messages(1).await;
    h.wait_delivered_cursor("refund-v2", 2).await;

    let mark = h.server.stream_opens().len();
    h.server.reset_log(None);
    let opens = eventually("refused, then reopened", WAIT, || {
        let o = h.server.stream_opens()[mark..].to_vec();
        o.iter().any(|o| o.accepted).then_some(o)
    })
    .await;
    assert!(opens[0].ahead && opens[0].after == 2, "{opens:?}");
    assert_eq!(opens.iter().find(|o| o.accepted).unwrap().after, 0);

    h.server.append(relayed_question("after the reset"));
    let m = inbox.wait_messages(2).await;
    assert!(m[1].contains("after the reset"), "{}", m[1]);
    assert!(pos(&m[1], 1).is_some());
    h.stop().await;
}

/// H4: an event this build cannot decode is skipped by seq, in the live
/// stream and in a gap-filling page, without a reconnect loop. An event
/// naming an unknown address form is never pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undecodable_events_are_skipped_not_retried() {
    let mut h = Harness::new().await;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    let mark = h.server.stream_opens().len();
    let broken = |v: &mut Value| v["id"] = json!("not-a-ulid");

    h.server.append(relayed_question("1: fine"));
    h.server
        .append_raw(relayed_question("2: broken, live"), true, broken);
    h.server.append(relayed_question("3: fine"));
    h.server
        .append_raw(relayed_question("4: broken, only in a page"), false, broken);
    h.server.append(relayed_question("5: fine, after a gap"));
    h.server
        .append_raw(relayed_question("6: names an unknown address"), true, |v| {
            v["to"] = json!(["senator::claude", "role:reviewer"])
        });
    h.server.append(relayed_question("7: fine"));

    h.wait_delivered_cursor("refund-v2", 7).await;
    let all = eventually("every good event pushed", WAIT, || {
        let text = inbox.messages().join("\n");
        [1, 3, 5, 7]
            .iter()
            .all(|s| pos(&text, *s).is_some())
            .then_some(text)
    })
    .await;
    for skipped in [2, 4, 6] {
        assert!(
            pos(&all, skipped).is_none(),
            "seq {skipped} must not be pushed: {all}"
        );
    }
    let opens = h.server.stream_opens()[mark..].to_vec();
    assert!(opens.is_empty(), "no reconnect loop: {opens:?}");
    // Marked delivered once the inbox's reply window has passed.
    eventually("nothing left undelivered", WAIT, || {
        h.store().undelivered().unwrap().is_empty().then_some(())
    })
    .await;
    h.stop().await;
}

/// A bridge joining a committee for the first time starts at the head;
/// history waits for catch_up. A long log's head is found by probing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_bridge_starts_at_the_head_not_at_history() {
    let mut h = Harness::with_committees(&["other"]).await;
    for i in 1..=3 {
        h.server.append(relayed_question(&format!("history {i}")));
    }
    let other = senate_core::CommitteeId::new("other").unwrap();
    for i in 1..=1203 {
        h.server.append(NewEvent::new(
            other.clone(),
            Kind::Note,
            person("pm"),
            format!("n{i}"),
        ));
    }
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.wait_stream("other").await;
    let first = |f: &str| {
        h.server
            .stream_opens()
            .into_iter()
            .find(|o| o.committee == f && o.accepted)
            .unwrap()
    };
    assert_eq!(first("refund-v2").after, 3);
    assert_eq!(first("other").after, 1203, "found by gallop and bisection");
    h.wait_delivered_cursor("refund-v2", 3).await;

    h.server.append(relayed_question("new"));
    let m = inbox.wait_messages(1).await;
    assert!(pos(&m[0], 4).is_some(), "{}", m[0]);
    for s in 1..=3 {
        assert!(pos(&m[0], s).is_none(), "history seq {s} pushed: {}", m[0]);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(inbox.messages().len(), 1);
    h.stop().await;
}

/// A fresh bridge for an agent whose delivered cursor the server already
/// holds resumes from it; a server cursor beyond the log is ignored. The
/// cursor is the agent's, not its person's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_bridge_resumes_from_the_servers_delivered_cursor() {
    let mut h = Harness::with_committees(&["other"]).await;
    for i in 1..=4 {
        h.server.append(relayed_question(&format!("q{i}")));
    }
    h.server
        .set_cursor("refund-v2", "claude", "delivered", Seq(2));
    // The person's own cursor and another agent's are not this agent's.
    h.server
        .set_cursor("refund-v2", "alice", "delivered", Seq(4));
    h.server
        .set_cursor("refund-v2", "codex", "delivered", Seq(4));
    let other = senate_core::CommitteeId::new("other").unwrap();
    for i in 1..=2 {
        h.server.append(NewEvent::new(
            other.clone(),
            Kind::Note,
            person("pm"),
            format!("n{i}"),
        ));
    }
    h.server.set_cursor("other", "claude", "delivered", Seq(99));
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    let out = h
        .hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    // The backlog arrives by the inbox, or, when the stream caught up before
    // the hook ran, in the hook's own context: either is a delivery. Hook
    // context comes first, so seq order holds across the two.
    let hooked = out
        .as_ref()
        .map(|o| context(o).to_string())
        .unwrap_or_default();
    let header = |n: u64| format!("[senate · refund-v2 · seq {n} ·");
    let both = eventually("seq 3 and 4 delivered", WAIT, || {
        let text = format!("{hooked}\n{}", inbox.messages().join("\n"));
        (text.contains(&header(3)) && text.contains(&header(4))).then_some(text)
    })
    .await;
    let at = |n: u64| both.find(&header(n));
    assert!(at(3).unwrap() < at(4).unwrap(), "{both}");
    assert!(at(1).is_none() && at(2).is_none(), "{both}");
    assert_eq!(
        both.matches(&header(3)).count(),
        1,
        "delivered once: {both}"
    );
    // Delivery on refund-v2 does not tell us when the other worker opens.
    h.wait_stream("other").await;
    let opens = h.server.stream_opens();
    let first = |f: &str| {
        opens
            .iter()
            .find(|o| o.committee == f && o.accepted)
            .unwrap()
    };
    assert_eq!(first("refund-v2").after, 2);
    assert_eq!(
        first("other").after,
        2,
        "a cursor beyond the log falls back to the head"
    );
    h.stop().await;
}

/// The inbox answers instead of staying silent, so the batch is not
/// counted as delivered. It is retried through the inbox after the backoff,
/// and a newer item waits behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_push_is_retried_through_the_inbox_in_order() {
    let mut h = Harness::new().await;
    h.start_with(senate_bridge::Timing {
        retry_base: Duration::from_secs(1),
        ..fast_timing()
    })
    .await;
    let inbox = FakeInbox::replying(
        h.dir.path().join("busy.sock"),
        &[r#"{"type":"error","message":"busy"}"#],
    );
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;

    h.server.append(relayed_question("first"));
    eventually("seq 1 held after the refusal", WAIT, || {
        let i = h.item(1)?;
        (i.status == ItemStatus::Queued && inbox.connections().len() == 1).then_some(())
    })
    .await;
    h.server.append(relayed_question("second"));
    let m = inbox.wait_messages(2).await;
    assert!(
        pos(&m[0], 1).is_some() && pos(&m[0], 2).is_none(),
        "{}",
        m[0]
    );
    let (p1, p2) = (pos(&m[1], 1).unwrap(), pos(&m[1], 2).unwrap());
    assert!(p1 < p2, "the held item goes first: {}", m[1]);
    // Marked delivered once the inbox's reply window has passed.
    eventually("both delivered through the inbox", WAIT, || {
        [1, 2]
            .iter()
            .all(|s| {
                h.item(*s)
                    .is_some_and(|i| i.status == ItemStatus::Delivered)
            })
            .then_some(())
    })
    .await;
    let t = inbox.times();
    assert!(
        t[1] - t[0] >= Duration::from_millis(800),
        "retried after the backoff: {:?}",
        t[1] - t[0]
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(inbox.connections().len(), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transport_failure_retries_without_further_activity() {
    let mut h = Harness::new().await;
    h.start_with(senate_bridge::Timing {
        retry_base: Duration::from_millis(300),
        ..fast_timing()
    })
    .await;
    let inbox = FakeInbox::replying(h.dir.path().join("busy.sock"), &["busy"]);
    let wd = h.workdir("idle", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "idle-1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("one question while idle"));

    let messages = inbox.wait_messages(2).await;
    assert_eq!(messages[0], messages[1]);
    eventually("delivered after the retry timer", WAIT, || {
        (h.item(1)?.status == ItemStatus::Delivered).then_some(())
    })
    .await;
    h.stop().await;
}

/// A backlog too big for one message goes out in several, spaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_big_backlog_is_split_and_the_pushes_are_spaced() {
    let mut h = Harness::new().await;
    // Well above the ~350 ms the reply window and batch window alone give.
    h.start_with(senate_bridge::Timing {
        push_spacing: Duration::from_secs(1),
        ..fast_timing()
    })
    .await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server.set_offline(true);
    eventually("the stream to drop", WAIT, || {
        (h.server.active_streams("refund-v2") == 0).then_some(())
    })
    .await;
    let n = MAX_BATCH_EVENTS as u64 + 5;
    for i in 1..=n {
        h.server.append(relayed_question(&format!("q{i}")));
    }
    h.server.set_offline(false);

    let msgs = eventually("every item pushed", WAIT, || {
        let m = inbox.messages();
        let text = m.join("\n");
        (1..=n).all(|s| pos(&text, s).is_some()).then_some(m)
    })
    .await;
    assert!(msgs.len() >= 2, "{} message(s)", msgs.len());
    let mut last = 0;
    for m in &msgs {
        let blocks = m.matches("[senate · refund-v2 · seq ").count();
        assert!(blocks <= MAX_BATCH_EVENTS, "{blocks} events in one push");
        for s in (1..=n).filter(|s| pos(m, *s).is_some()) {
            assert!(s > last, "seq {s} out of order");
            last = s;
        }
    }
    let t = inbox.times();
    for pair in t.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(800),
            "pushes {:?} apart",
            pair[1] - pair[0]
        );
    }
    h.stop().await;
}

/// With no session bound, a committee keeps its newest items only, and
/// the next hand-over says how many were dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unbound_committee_keeps_only_its_newest_items() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    // An agent on record (its session ended) keeps the configured stream.
    let wd = h.workdir("late", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "first", &wd, None).await;
    end_session(&h, "first", &wd).await;
    h.wait_stream("refund-v2").await;
    let mut watch = IpcClient::connect(&h.socket(), WAIT)
        .await
        .unwrap()
        .watch(false, WAIT)
        .await
        .unwrap();
    let n = PENDING_CAP as u64 + 5;
    for i in 1..=n {
        h.server.append(relayed_question(&format!("q{i}")));
    }
    // An Inbox notice for a later undirected event proves that this stream
    // has finished processing every earlier event, including queue trimming.
    let barrier = h.server.append(note(person("pm"), "queue intake barrier"));
    let me = path("claude");
    tokio::time::timeout(WAIT, async {
        loop {
            let event = watch
                .next()
                .await
                .unwrap()
                .expect("watch closed before the intake barrier");
            if matches!(event, WatchEvent::Inbox { agent, committee, seq, .. }
                if agent == me && committee == barrier.committee && seq == barrier.seq)
            {
                break;
            }
        }
    })
    .await
    .expect("agent stream reached the intake barrier");
    assert_eq!(
        h.store().delivered_cursor(&me, &committee()).unwrap(),
        barrier.seq
    );
    let pending = h.store().pending().unwrap();
    assert_eq!(pending.len(), PENDING_CAP);
    assert_eq!(pending[0].event.seq, Seq(6));
    assert_eq!(h.store().dropped(&me, &committee()).unwrap(), 5);

    let out = h
        .hook(HookKind::SessionStart, "late", &wd, None)
        .await
        .unwrap();
    let ctx = context(&out);
    assert!(ctx.contains("5 older message(s)"), "{ctx}");
    assert!(ctx.contains("mcp__senate__catch_up"));
    assert!(pos(ctx, 6).is_some() && pos(ctx, 5).is_none(), "{ctx}");
    assert_eq!(h.store().dropped(&me, &committee()).unwrap(), 0);
    h.stop().await;
}

async fn end_session(h: &Harness, id: &str, wd: &std::path::Path) {
    h.hook(HookKind::SessionEnd, id, wd, None).await;
    eventually(&format!("{id} ended"), WAIT, || {
        (h.store().session(id).unwrap()?.state == SessionState::Ended).then_some(())
    })
    .await;
}

async fn wait_pending(h: &Harness, seq: u64) {
    eventually(&format!("seq {seq} pending"), WAIT, || {
        let item = h.item(seq)?;
        (item.status == ItemStatus::Pending).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_injects_the_brief_ahead_of_the_backlog() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    let wd = h.workdir("b", Some("refund-v2"));
    let out = h
        .hook(HookKind::SessionStart, "q", &wd, None)
        .await
        .unwrap();
    let ctx = context(&out);
    assert!(ctx.starts_with("[senate] This session is joined"));
    assert!(ctx.contains("mcp__senate__propose_decision"));
    assert!(ctx.contains("# Brief: refund-v2 (through seq 0)"), "{ctx}");
    assert!(ctx.contains("None recorded yet."));
    h.wait_stream("refund-v2").await;

    // A decision (undirected: never pushed), then a question for alice that
    // waits for the next session to start.
    end_session(&h, "q", &wd).await;
    h.server.append(NewEvent::new(
        committee(),
        Kind::Decision,
        person("pm"),
        "退款上限 30 天",
    ));
    h.server.append(relayed_question("还缺什么？"));
    wait_pending(&h, 2).await;
    let out = h
        .hook(HookKind::SessionStart, "r", &wd, None)
        .await
        .unwrap();
    let ctx = context(&out);
    let orientation = ctx.find("[senate] This session is joined").unwrap();
    let brief = ctx.find("# Brief: refund-v2 (through seq 2)").unwrap();
    let decision = ctx.find("- seq 1 · ").unwrap();
    let backlog = ctx.find("[senate] The messages below").unwrap();
    assert!(
        orientation < brief && brief < decision && decision < backlog,
        "{ctx}"
    );
    assert!(ctx.contains("[senator::pm]: 退款上限 30 天"), "{ctx}");
    assert!(ctx.contains("还缺什么？"));
    assert!(ctx.chars().count() <= 10_000);

    // The brief cannot be had: the session starts as it did before briefs.
    h.server.set_brief_broken(true);
    end_session(&h, "r", &wd).await;
    h.server.append(relayed_question("还在吗？"));
    wait_pending(&h, 3).await;
    let out = h
        .hook(HookKind::SessionStart, "s", &wd, None)
        .await
        .expect("a failed brief does not fail the hook");
    let ctx = context(&out);
    assert!(ctx.starts_with("[senate] This session is joined"));
    assert!(!ctx.contains("# Brief"), "{ctx}");
    assert!(ctx.contains("还在吗？"));

    // Nor does a slow one: the hook answers in time, without it.
    h.server.set_brief_broken(false);
    h.server.set_brief_delay(Duration::from_secs(5));
    let started = std::time::Instant::now();
    let out = h
        .hook(HookKind::SessionStart, "t", &wd, None)
        .await
        .expect("a slow brief does not fail the hook");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!context(&out).contains("# Brief"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn brief_chars_bounds_the_injected_brief() {
    let mut h = Harness::new().await;
    for i in 0..30 {
        h.server.append(NewEvent::new(
            committee(),
            Kind::Decision,
            person("pm"),
            format!("decision {i} {}", "x".repeat(200)),
        ));
    }
    h.config.brief_chars = 1_500;
    h.start().await;
    let wd = h.workdir("c", Some("refund-v2"));
    let out = h
        .hook(HookKind::SessionStart, "c", &wd, None)
        .await
        .unwrap();
    let ctx = context(&out);
    let brief = &ctx[ctx.find("[senate] The brief of `refund-v2`").unwrap()..];
    assert!(brief.chars().count() <= 1_500, "{}", brief.chars().count());
    assert!(brief.contains("decision 29"));
    assert!(brief.contains("older decisions omitted"));
    h.stop().await;

    let mut h = Harness::new().await;
    h.config.brief_chars = 0;
    h.start().await;
    let wd = h.workdir("d", Some("refund-v2"));
    let out = h
        .hook(HookKind::SessionStart, "d", &wd, None)
        .await
        .unwrap();
    assert!(!context(&out).contains("# Brief"));
    h.stop().await;
}

/// Two agents of one person on one machine, on one committee and one
/// branch: a Claude Code session is `claude`, a Codex session
/// `codex`. Each has its own stream, cursor and heartbeat, and a
/// message for one reaches only that agent's session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_of_one_person_have_their_own_streams_cursors_and_pushes() {
    let mut h = Harness::new().await;
    let codex_server = h.codex();
    h.start().await;
    // One worktree on the person's branch, shared by both agents.
    let wd = h.workdir("shared", Some("refund-v2"));
    let inbox = h.inbox("cc");
    let start = h
        .hook(HookKind::SessionStart, "cc", &wd, Some(&inbox))
        .await
        .unwrap();
    assert!(context(&start).contains("as [senator::claude]"));
    let start = h
        .hook_codex(HookKind::SessionStart, "thread-1", &wd)
        .await
        .unwrap();
    assert!(context(&start).contains("as [senator::codex]"));
    codex_server.idle("thread-1");
    h.wait_stream_of("refund-v2", "claude").await;
    h.wait_stream_of("refund-v2", "codex").await;
    let cc = h.store().session("cc").unwrap().unwrap();
    let cx = h.store().session("thread-1").unwrap().unwrap();
    assert_eq!(cc.agent, path("claude"));
    assert_eq!(cx.agent, path("codex"));

    // 1: for claude only; 2: for codex only; 3: for both.
    h.server
        .append(relayed_to(claude("alice"), "1: claude, which header?"));
    h.server
        .append(relayed_to(codex("alice"), "2: codex, which header?"));
    h.server
        .append(relayed_to(claude("alice"), "3: both of you").to(codex("alice")));

    let claude_text = eventually("1 and 3 in the Claude inbox", WAIT, || {
        let t = inbox.messages().join("\n");
        (pos(&t, 1).is_some() && pos(&t, 3).is_some()).then_some(t)
    })
    .await;
    let codex_text = eventually("2 and 3 through the app-server", WAIT, || {
        let t = codex_server
            .pushes()
            .iter()
            .map(|(_, p)| input_text(p).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        (pos(&t, 2).is_some() && pos(&t, 3).is_some()).then_some(t)
    })
    .await;
    assert!(pos(&claude_text, 2).is_none(), "{claude_text}");
    assert!(pos(&codex_text, 1).is_none(), "{codex_text}");
    assert_eq!(claude_text.matches("· seq 3 ·").count(), 1);
    assert_eq!(codex_text.matches("· seq 3 ·").count(), 1);

    // Each agent's queue row is its own.
    eventually("every row delivered", WAIT, || {
        let done = |a: &str, s: u64| {
            h.item_of(a, s)
                .is_some_and(|i| i.status == ItemStatus::Delivered)
        };
        (done("claude", 1) && done("claude", 3) && done("codex", 2) && done("codex", 3))
            .then_some(())
    })
    .await;
    assert!(h.item_of("claude", 2).is_none());
    assert!(h.item_of("codex", 1).is_none());

    // Two cursors on the server, two heartbeats, two streams: none shared,
    // and the person's own cursor is never written.
    h.wait_delivered_cursor_of("refund-v2", "claude", 3).await;
    h.wait_delivered_cursor_of("refund-v2", "codex", 3).await;
    assert_eq!(h.server.cursor("refund-v2", "alice", "delivered"), None);
    eventually("a heartbeat from each agent", WAIT, || {
        let claude = h.server.heartbeats_of("refund-v2", "claude");
        let codex = h.server.heartbeats_of("refund-v2", "codex");
        (claude
            .iter()
            .any(|hb| hb.sessions == 1 && hb.cursor == Seq(3))
            && codex
                .iter()
                .any(|hb| hb.sessions == 1 && hb.cursor == Seq(3)))
        .then_some(())
    })
    .await;
    let senators: std::collections::BTreeSet<Option<String>> = h
        .server
        .stream_opens()
        .into_iter()
        .map(|o| o.senator)
        .collect();
    assert_eq!(
        senators,
        [Some("claude".to_string()), Some("codex".to_string())]
            .into_iter()
            .collect()
    );

    // Ending the Codex session closes only its agent's stream.
    h.hook_codex(HookKind::SessionEnd, "thread-1", &wd).await;
    eventually("codex's stream to close", WAIT, || {
        (h.server.active_streams_of("refund-v2", "codex") == 0).then_some(())
    })
    .await;
    assert!(h.server.active_streams_of("refund-v2", "claude") > 0);
    h.stop().await;
}

/// A session pins its agent with `SENATE_AGENT` or `.senate/agent`;
/// `[agents]` renames the harness default. A pinned agent gets its own
/// stream and only its own messages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_can_pin_its_agent_and_the_config_can_rename_one() {
    let mut h = Harness::new().await;
    h.config.agents.claude = "cc".parse().unwrap();
    h.start().await;
    let plain = h.workdir("plain", Some("refund-v2"));
    let inbox_cc = h.inbox("cc");
    h.hook(HookKind::SessionStart, "cc", &plain, Some(&inbox_cc))
        .await;
    assert_eq!(h.store().session("cc").unwrap().unwrap().agent, path("cc"));

    // By environment.
    let inbox_env = h.inbox("env");
    let mut env = h.env_for("env", Some(&inbox_env.path));
    env.agent = Some("reviewer".into());
    h.hook_with(HookKind::SessionStart, "env", &plain, &env)
        .await;
    // By worktree file.
    let pinned = h.workdir("pinned", Some("refund-v2"));
    std::fs::write(pinned.join(".senate/agent"), "# this worktree\nfrontend\n").unwrap();
    let inbox_file = h.inbox("file");
    h.hook(HookKind::SessionStart, "file", &pinned, Some(&inbox_file))
        .await;
    let agent_of = |id: &str| h.store().session(id).unwrap().unwrap();
    assert_eq!(agent_of("env").agent, path("reviewer"));
    assert_eq!(
        agent_of("env").agent_source,
        senate_bridge::agent::AgentSource::Env
    );
    assert_eq!(agent_of("file").agent, path("frontend"));
    assert_eq!(
        agent_of("file").agent_source,
        senate_bridge::agent::AgentSource::File
    );
    for agent in ["cc", "reviewer", "frontend"] {
        h.wait_stream_of("refund-v2", agent).await;
    }

    h.server
        .append(relayed_to(senator("frontend"), "for the frontend agent"));
    let m = inbox_file.wait_messages(1).await;
    assert!(m[0].contains("for the frontend agent"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(inbox_cc.messages().is_empty() && inbox_env.messages().is_empty());
    h.stop().await;
}

/// `/v1/me` names the Senate by its path: the hook's orientation and
/// status say which Senate this bridge is connected to, and an agent named
/// in `[agents]` is one flat name in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_senate_is_named_by_its_path_and_agents_by_flat_names() {
    let mut h = Harness::new().await;
    h.server.set_senate("north-desk", Some("acme"));
    h.config.agents.claude = "reviewer".parse().unwrap();
    h.start().await;
    let wd = h.workdir("n", Some("refund-v2"));
    let inbox = h.inbox("n");
    let out = h
        .hook(HookKind::SessionStart, "n", &wd, Some(&inbox))
        .await
        .unwrap();
    assert!(
        context(&out)
            .contains("joined to the committee `refund-v2` of the Senate [/acme/north-desk] as [senator::reviewer]"),
        "{}",
        context(&out)
    );
    h.wait_stream_of("refund-v2", "reviewer").await;
    let status: StatusReply = ipc::request(
        &h.socket(),
        &Request::Status,
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(status.senate, Some("/acme/north-desk".parse().unwrap()));
    h.server.append(relayed_to(
        senator("reviewer"),
        "a question for the reviewer",
    ));
    let m = inbox.wait_messages(1).await;
    assert!(m[0].contains("a question for the reviewer"));
    h.stop().await;
}

/// The bridge's locale decides every word it writes for the agent; the ids
/// and kinds stay the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_and_hook_context_follow_the_locale() {
    let mut h = Harness::new().await;
    h.config.locale = senate_core::Locale::Zh;
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    let start = h
        .hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await
        .unwrap();
    let ctx = context(&start);
    assert!(
        ctx.contains("[senate] 这个会话已加入 Senate [/senate] 的委员会 `refund-v2`，身份是 [senator::claude]"),
        "{ctx}"
    );
    assert!(ctx.contains("# 简报：refund-v2（截至 seq 0）"), "{ctx}");
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("退款上限是多少天？"));
    let m = inbox.wait_messages(1).await;
    assert!(m[0].starts_with("[senate] 下面的消息来自 Senate 委员会 refund-v2 的日志"));
    assert!(m[0].contains(
        "[senate · refund-v2 · seq 1 · 来自 [speaker::pm] · question]\n\
         │ 退款上限是多少天？\n\
         ↳ 用 mcp__senate__answer(thread=1) 回复——会发回给 [senator::pm]"
    ));
    h.stop().await;
}

/// One daemon serves one Senate. A session pinned to a path (what used to
/// select a Senate presided over by an agent) is not sent anywhere else:
/// the pin is ignored and the session speaks as its harness's agent, on
/// the same server, in the same store, as every other session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_daemon_serves_one_senate_and_a_pinned_path_is_ignored() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("w", Some("refund-v2"));
    let up = h.inbox("c1");
    let down = h.inbox("r1");
    let mut env = h.env_for("r1", Some(down.path.as_path()));
    env.agent = Some("codex".into());
    let start = h
        .hook_with(HookKind::SessionStart, "r1", &wd, &env)
        .await
        .unwrap();
    assert!(
        context(&start).contains("as [senator::codex]"),
        "{}",
        context(&start)
    );
    let mut env = h.env_for("c1", Some(up.path.as_path()));
    env.agent = Some("claude/reviewer".into());
    let start = h
        .hook_with(HookKind::SessionStart, "c1", &wd, &env)
        .await
        .unwrap();
    assert!(
        context(&start).contains("of the Senate [/senate] as [senator::claude]"),
        "{}",
        context(&start)
    );
    let daemon = h.daemon.as_ref().unwrap();
    let c1 = daemon.store().session("c1").unwrap().unwrap();
    assert_eq!(c1.agent, path("claude"));
    assert_eq!(c1.agent_source, senate_bridge::agent::AgentSource::Harness);
    assert_eq!(
        daemon.store().session("r1").unwrap().unwrap().agent,
        path("codex")
    );

    // Both agents' streams and heartbeats go to the one server.
    eventually("both streams", WAIT, || {
        (h.server.active_streams_of("refund-v2", "claude") == 1
            && h.server.active_streams_of("refund-v2", "codex") == 1)
            .then_some(())
    })
    .await;
    eventually("heartbeats", WAIT, || {
        (!h.server.heartbeats_of("refund-v2", "claude").is_empty()
            && !h.server.heartbeats_of("refund-v2", "codex").is_empty())
        .then_some(())
    })
    .await;

    // A question to codex reaches its session only; its answer lands on
    // the same server.
    h.server.append(
        NewEvent::new(committee(), Kind::Question, claude("bob"), "回调覆盖了吗？")
            .to(senator("codex")),
    );
    let pushed = eventually("codex's push", WAIT, || {
        down.messages()
            .into_iter()
            .find(|m| m.contains("回调覆盖了吗"))
    })
    .await;
    assert!(pushed.contains("seq 1"), "{pushed}");
    assert!(up.messages().iter().all(|m| !m.contains("回调覆盖了吗")));
    let posted: ipc::Posted = call(
        &h,
        Request::Answer {
            session: ipc::SessionSelector {
                session_id: Some("r1".into()),
                claude_pid: None,
            },
            thread: Seq(1),
            body: "覆盖了".into(),
            id: None,
            committee: None,
            agent: None,
            as_person: false,
        },
    )
    .await;
    assert_eq!(posted.seq, Seq(2));
    let answer = h.server.last_event("refund-v2");
    assert_eq!(
        (answer.body.as_str(), &answer.from),
        ("覆盖了", &senator("codex"))
    );

    let status: StatusReply = call(&h, Request::Status).await;
    assert_eq!(status.senate, Some("/senate".parse().unwrap()));
    assert_eq!(status.server_url, h.server.url);
    h.stop().await;
}

/// The person writes to a qualified name (`ibot::senator::codex`): the
/// daemon asks the server where it is (`GET /v1/resolve`, standing on the
/// committee the person named) and posts on that committee, to that seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_qualified_name_is_resolved_by_the_server_and_posted_on_its_committee() {
    let mut h = Harness::new().await;
    h.start().await;
    h.server.seat("codex", "alice");
    h.server.append(NewEvent::new(
        senate_core::CommitteeId::new("ibot").unwrap(),
        Kind::Note,
        person("bob"),
        "ibot starts",
    ));
    let posted: ipc::Posted = call(
        &h,
        Request::Say {
            session: ipc::SessionSelector::default(),
            committee: Some(committee()),
            kind: ipc::SayKind::Question,
            to: Vec::new(),
            at: Some("ibot::senator::codex".parse().unwrap()),
            body: "is the ibot hook on?".into(),
            thread: None,
            id: None,
        },
    )
    .await;
    assert_eq!(posted.committee.as_str(), "ibot");
    assert_eq!(posted.to, [senator("codex")]);
    assert_eq!(
        h.server.resolves(),
        [(
            "ibot::senator::codex".to_string(),
            Some("refund-v2".to_string())
        )]
    );
    let sent = h.server.last_event("ibot");
    assert_eq!(sent.kind, Kind::Question);
    assert_eq!(sent.from, person("alice"));
    assert_eq!(sent.to, [senator("codex")]);
    assert_eq!(sent.body, "is the ibot hook on?");
    assert!(
        h.server.events("refund-v2").is_empty(),
        "nothing on the committee written from"
    );
    h.stop().await;
}

/// A qualified name the server does not know is refused, in the person's
/// language, and nothing is posted: a Senate path that is not this one's
/// (no Senate above to ask), and a senator nobody knows on a committee
/// that exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_qualified_name_that_is_elsewhere_or_unknown_is_refused_readably() {
    for locale in [senate_core::Locale::En, senate_core::Locale::Zh] {
        let mut h = Harness::new().await;
        h.config.locale = locale;
        h.start().await;
        h.server.append(NewEvent::new(
            senate_core::CommitteeId::new("ibot").unwrap(),
            Kind::Note,
            person("bob"),
            "ibot starts",
        ));
        let say = |at: &str| Request::Say {
            session: ipc::SessionSelector::default(),
            committee: Some(committee()),
            kind: ipc::SayKind::Note,
            to: Vec::new(),
            at: Some(at.parse().unwrap()),
            body: "hello".into(),
            thread: None,
            id: None,
        };
        let refused = |request: Request| {
            let socket = h.socket();
            async move {
                let err = ipc::request::<ipc::Posted>(
                    &socket,
                    &request,
                    Duration::from_secs(1),
                    Duration::from_secs(5),
                )
                .await
                .unwrap_err();
                let ipc::ClientError::Remote(e) = err else {
                    panic!("{err}")
                };
                e.message
            }
        };
        let elsewhere = refused(say("elsewhere::ibot::senator::codex")).await;
        let unknown = refused(say("ibot::senator::nobody")).await;
        match locale {
            senate_core::Locale::En => {
                assert_eq!(
                    elsewhere,
                    "[elsewhere::ibot::senator::codex] is not in this Senate, nor in any \
                     Senate above it knows of."
                );
                assert!(
                    unknown.starts_with("[ibot::senator::nobody] cannot be reached: ")
                        && unknown.contains("has no senator nobody"),
                    "{unknown}"
                );
            }
            senate_core::Locale::Zh => {
                assert_eq!(
                    elsewhere,
                    "[elsewhere::ibot::senator::codex] 不在这个 Senate，也不在它知道的任何上一级 Senate 里。"
                );
                assert!(
                    unknown.starts_with("[ibot::senator::nobody] 无法送达："),
                    "{unknown}"
                );
            }
        }
        assert_eq!(h.server.events("ibot").len(), 1, "nothing posted");
        assert!(h.server.events("refund-v2").is_empty(), "nothing posted");
        h.stop().await;
    }
}

async fn call<T: serde::de::DeserializeOwned>(h: &Harness, request: Request) -> T {
    ipc::request(
        &h.socket(),
        &request,
        Duration::from_secs(1),
        Duration::from_secs(10),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_duty_is_posted_once_where_the_seat_has_none() {
    let mut h = Harness::new().await;
    h.config.agent.duty = Some("  reviews\nthe gateway ".into());
    // claude's seat already has a duty on `other`, and none on refund-v2.
    h.server.append(
        senate_core::DutyPayload {
            senator: "claude".parse().unwrap(),
            duty: "owns the app".into(),
        }
        .note(
            senate_core::CommitteeId::new("other").unwrap(),
            claude("alice"),
        ),
    );
    h.start().await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    let posted = eventually("the default duty", WAIT, || {
        h.server
            .events("refund-v2")
            .into_iter()
            .find(|e| e.meta.contains_key(senate_core::meta::DUTY))
    })
    .await;
    assert_eq!(posted.from, claude("alice"));
    assert_eq!(
        posted.meta[senate_core::meta::DUTY],
        json!({"senator": "claude", "duty": "reviews the gateway"})
    );

    // On `other`, the seat's own duty wins: nothing is posted.
    let wd2 = h.workdir("w2", Some("other"));
    h.hook(HookKind::SessionStart, "s2", &wd2, Some(&h.inbox("s2")))
        .await;
    h.wait_stream("other").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let duties: Vec<_> = h
        .server
        .events("other")
        .into_iter()
        .filter(|e| e.meta.contains_key(senate_core::meta::DUTY))
        .collect();
    assert_eq!(duties.len(), 1, "{duties:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_older_servers_aliases_are_read_senate_wide_and_cannot_be_removed() {
    use senate_core::alias::KnownAliases;

    let mut h = Harness::new().await;
    {
        let mut inner = h.server.state.inner.lock().unwrap();
        inner.old_aliases = true;
        inner
            .aliases
            .set(
                &senate_core::CommitteeId::new("refund-v2").unwrap(),
                "main".parse().unwrap(),
                "claude-old".parse().unwrap(),
            )
            .unwrap();
    }
    h.start().await;
    let timeout = Duration::from_secs(5);
    let known: KnownAliases = ipc::request(&h.socket(), &Request::Aliases, timeout, timeout)
        .await
        .unwrap();
    let KnownAliases::Unscoped(map) = known else {
        panic!("an older server's aliases are Senate-wide: {known:?}");
    };
    assert_eq!(map[&"main".parse().unwrap()].as_str(), "claude-old");

    // This server has no route to remove an alias.
    let refused = ipc::request::<Value>(
        &h.socket(),
        &Request::RemoveAlias {
            primary: "claude-old".parse().unwrap(),
            alias: "main".parse().unwrap(),
            committee: None,
        },
        timeout,
        timeout,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(refused.contains("too old for senator unalias"), "{refused}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agy_session_takes_its_messages_as_a_user_message_before_its_next_model_call() {
    use senate_bridge::session::Runtime;
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    let wd = h.workdir("agy", Some("refund-v2"));
    let id = "5d0f8e8e-56b4-4c3a-8d0e-2b2f3c4d5e6f";
    let input = json!({"conversationId": id, "cwd": wd}).to_string();
    let hook = |kind| {
        let input = input.clone();
        let socket = h.socket();
        async move {
            senate_bridge::hook::run_as(
                kind,
                &socket,
                &input,
                &Default::default(),
                Some(Runtime::Agy),
                Default::default(),
            )
            .await
            .map(|out| serde_json::from_str::<Value>(&out).unwrap())
        }
    };
    let start = hook(HookKind::SessionStart).await.unwrap();
    let steps = start["injectSteps"].as_array().unwrap();
    // Only Senate's own identity line has system authority.
    assert!(
        steps[0]["systemMessage"]["systemMessage"]
            .as_str()
            .unwrap()
            .starts_with("[senate] refund-v2 · [senator::agy-"),
        "{start}"
    );
    assert!(
        steps[1]["userMessage"]
            .as_str()
            .unwrap()
            .starts_with("[senate] This session is joined"),
        "{start}"
    );
    h.wait_stream("refund-v2").await;
    let agent = format!("agy-{id}");
    let asked = h.server.append(relayed_to(
        Address::senator(&path(&agent)),
        "waiting for your next model call",
    ));
    eventually("the item held for hooks", WAIT, || {
        let item = h.item_of(&agent, asked.seq.0)?;
        (item.status == ItemStatus::Queued).then_some(())
    })
    .await;
    let out = hook(HookKind::UserPromptSubmit)
        .await
        .expect("the queue comes back");
    let steps = out["injectSteps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "{out}");
    let text = steps[0]["userMessage"].as_str().unwrap();
    assert!(text.starts_with("[senate] The messages below"), "{text}");
    assert!(text.contains("waiting for your next model call"), "{text}");
    assert!(
        hook(HookKind::UserPromptSubmit).await.is_none(),
        "handed over once"
    );
    h.stop().await;
}

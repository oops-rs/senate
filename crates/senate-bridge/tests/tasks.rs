//! Task threads in the daemon: triage gating, takeover, thread owners
//! (one session of one agent), and reassigned work.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use senate_bridge::hook::HookKind;
use senate_bridge::state::ItemStatus;
use senate_core::{
    Address, Kind, Locale, NewEvent, Refs, Seq, StatusPayload, StatusState, TriagePayload,
    TriageVerdict, meta,
};

fn gated(wait: Duration) -> senate_bridge::daemon::Timing {
    senate_bridge::daemon::Timing {
        triage_wait: wait,
        ..fast_timing()
    }
}

fn verdict(root: Seq, v: TriageVerdict) -> NewEvent {
    let triage = TriagePayload {
        verdict: v,
        p_needs_human: 0.2,
        p_agent: 0.7,
        p_unsure: 0.1,
        rule: None,
        reason: Some("查代码就能答".into()),
    };
    NewEvent::new(committee(), Kind::Note, Address::Speaker, "")
        .in_thread(root)
        .with_meta(meta::TRIAGE, triage.value())
}

/// alice/claude's session, joined; pushes in Chinese, which these tests
/// read.
async fn joined(h: &mut Harness, timing: senate_bridge::daemon::Timing) -> FakeInbox {
    h.config.locale = Locale::Zh;
    h.start_with(timing).await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    inbox
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_waits_for_the_verdict_then_goes() {
    let mut h = Harness::new().await;
    let inbox = joined(&mut h, gated(Duration::from_secs(30))).await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "用哪个 header？"));
    // A Speaker relay is judged already, but it waits with the question so
    // the session gets one message in seq order.
    h.server.append(relayed_question("退款上限？"));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        inbox.messages().is_empty(),
        "the session's push waits for the verdict"
    );

    h.server.append(verdict(q.seq, TriageVerdict::Agent));
    let msgs = inbox.wait_messages(1).await;
    let (p1, p2) = (pos(&msgs[0], 1).unwrap(), pos(&msgs[0], 2).unwrap());
    assert!(p1 < p2, "seq order: {}", msgs[0]);
    assert!(!msgs[0].contains("本人确认"), "{}", msgs[0]);
    h.stop().await;
}

/// Where the server says no Speaker judges roots (a server with none, a
/// nested Senate), a root is pushed at once: no verdict will ever come.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_does_not_wait_where_nothing_judges() {
    let mut h = Harness::new().await;
    h.server.state.inner.lock().unwrap().triaged = false;
    let inbox = joined(&mut h, gated(Duration::from_secs(30))).await;
    h.server
        .append(question(claude("bob"), claude("alice"), "用哪个 header？"));
    let msgs = inbox.wait_messages(1).await;
    assert!(msgs[0].contains("用哪个 header"), "{}", msgs[0]);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_verdict_in_time_goes_flagged() {
    let mut h = Harness::new().await;
    let inbox = joined(&mut h, gated(Duration::from_millis(400))).await;
    h.server
        .append(question(claude("bob"), claude("alice"), "能延期吗？"));
    let msgs = inbox.wait_messages(1).await;
    assert!(
        msgs[0].contains("Speaker 认为可能需要你本人确认"),
        "{}",
        msgs[0]
    );
    assert!(msgs[0].contains("mcp__senate__escalate(thread=1)"));
    h.stop().await;

    // The same in English.
    let mut h = Harness::new().await;
    h.start_with(gated(Duration::from_millis(400))).await;
    let inbox = h.inbox("s1");
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&inbox))
        .await;
    h.wait_stream("refund-v2").await;
    h.server
        .append(question(claude("bob"), claude("alice"), "can it slip?"));
    let msgs = inbox.wait_messages(1).await;
    assert!(
        msgs[0].contains(
            "↳ the Speaker thinks this may need your user (the Speaker gave no verdict in time)"
        ),
        "{}",
        msgs[0]
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn needs_human_is_never_pushed_and_the_approval_is() {
    let mut h = Harness::new().await;
    let inbox = joined(&mut h, gated(Duration::from_secs(30))).await;
    let r = h.server.append(
        NewEvent::new(
            committee(),
            Kind::Request,
            claude("bob"),
            "把 header 改成 Idempotency-Key",
        )
        .to(claude("alice")),
    );
    h.server.append(verdict(r.seq, TriageVerdict::NeedsHuman));
    eventually("the request is cancelled", WAIT, || {
        let item = h.item(r.seq.0)?;
        (item.status == ItemStatus::Cancelled).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(inbox.messages().is_empty());

    // Alice approves in the committee's chat: it goes to her own agent.
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Request,
            person("alice"),
            "把 header 改成 Idempotency-Key",
        )
        .in_thread(r.seq)
        .to(claude("alice")),
    );
    let msgs = inbox.wait_messages(1).await;
    assert!(pos(&msgs[0], 3).is_some(), "{}", msgs[0]);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_cancels_what_is_still_waiting() {
    let mut h = Harness::new().await;
    let inbox = joined(&mut h, gated(Duration::from_secs(30))).await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "用哪个 header？"));
    let mut takeover = StatusPayload::new(StatusState::Suspended);
    takeover.takeover = Some(name("alice"));
    h.server.append(
        NewEvent::new(committee(), Kind::Status, Address::Speaker, "")
            .in_thread(q.seq)
            .with_meta(meta::STATUS, takeover.value()),
    );
    eventually("the question is cancelled", WAIT, || {
        let item = h.item(q.seq.0)?;
        (item.status == ItemStatus::Cancelled).then_some(())
    })
    .await;
    // A late verdict does not bring it back.
    h.server.append(verdict(q.seq, TriageVerdict::Agent));
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(inbox.messages().is_empty());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_that_took_a_root_gets_its_thread() {
    let mut h = Harness::new().await;
    h.start().await;
    let first = h.inbox("s1");
    let second = h.inbox("s2");
    let w1 = h.workdir("w1", Some("refund-v2"));
    let w2 = h.workdir("w2", Some("refund-v2"));
    std::fs::create_dir_all(w2.join("gateway")).unwrap();
    h.hook(HookKind::SessionStart, "s1", &w1, Some(&first))
        .await;
    h.wait_stream("refund-v2").await;
    let q = h.server.append(relayed_question("退款上限？"));
    first.wait_messages(1).await;
    // s2 is newer and its checkout holds the path the follow-up names, so
    // without an owner it would win.
    h.hook(HookKind::SessionStart, "s2", &w2, Some(&second))
        .await;
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Chat,
            person("pm"),
            "补充：gateway 那边也要改",
        )
        .in_thread(q.seq)
        .to(claude("alice"))
        .with_refs(Refs {
            path: Some("gateway".into()),
            ..Refs::default()
        }),
    );
    let msgs = first.wait_messages(2).await;
    assert!(pos(&msgs[1], 2).is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(second.messages().is_empty());
    let owner = h.store().owner(&committee(), q.seq).unwrap().unwrap();
    assert_eq!(
        (owner.agent.to_string(), owner.session.as_str()),
        ("alice/claude".to_string(), "s1")
    );
    h.stop().await;
}

/// A thread's owner is one session *of one agent*: a follow-up in that
/// thread for another agent of the same person goes to that agent's own
/// session, never to the owner's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thread_owner_does_not_take_another_agents_follow_ups() {
    let mut h = Harness::new().await;
    h.start().await;
    let claude_inbox = h.inbox("s1");
    let reviewer_inbox = h.inbox("s2");
    let wd = h.workdir("w", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, Some(&claude_inbox))
        .await;
    let mut env = h.env_for("s2", Some(&reviewer_inbox.path));
    env.agent = Some("reviewer".into());
    h.hook_with(HookKind::SessionStart, "s2", &wd, &env).await;
    h.wait_stream_of("refund-v2", "alice/claude").await;
    h.wait_stream_of("refund-v2", "alice/reviewer").await;

    let q = h.server.append(relayed_question("退款上限？"));
    claude_inbox.wait_messages(1).await;
    h.server.append(
        NewEvent::new(committee(), Kind::Chat, person("pm"), "reviewer 也看一下")
            .in_thread(q.seq)
            .to(senator("alice/reviewer")),
    );
    let m = reviewer_inbox.wait_messages(1).await;
    assert!(pos(&m[0], 2).is_some(), "{}", m[0]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(claude_inbox.messages().len(), 1);
    h.stop().await;
}

fn reassigned(who: &str, owner: Option<&str>, ms: i64) -> NewEvent {
    let payload = senate_core::PresencePayload {
        agent: path(who),
        state: if owner.is_some() {
            senate_core::PresenceState::Reassigned
        } else {
            senate_core::PresenceState::Online
        },
        since: chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + ms).unwrap(),
        since_seq: Seq(0),
        previous: None,
        last_seen: None,
        cursor: None,
        lag: None,
        new_owner: owner.map(path),
    };
    senate_core::presence_event(committee(), &payload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reassigned_work_reaches_the_new_owner_until_handed_back() {
    let mut h = Harness::new().await;
    let inbox = joined(&mut h, gated(Duration::from_millis(200))).await;
    // bob/claude's work is handed to alice/claude (this machine).
    h.server
        .append(reassigned("bob/claude", Some("alice/claude"), 0));
    // bob's other agent was not reassigned: its messages stay its own.
    h.server.append(question(
        Address::Speaker,
        senator("bob/codex"),
        "codex 那边呢？",
    ));
    let q = h.server.append(question(
        Address::Speaker,
        claude("bob"),
        "重试路径做到哪了？",
    ));
    let msgs = inbox.wait_messages(1).await;
    assert!(pos(&msgs[0], q.seq.0).is_some(), "{}", msgs[0]);
    assert!(pos(&msgs[0], 2).is_none(), "{}", msgs[0]);
    assert!(
        msgs[0].contains("给 [senator::bob/claude] 的消息（你已接手）"),
        "{}",
        msgs[0]
    );
    // Stored as it was: still to senator::bob/claude.
    assert_eq!(h.server.events("refund-v2")[2].to, vec![claude("bob")]);

    // Handed back: bob's messages are his again.
    h.server.append(reassigned("bob/claude", None, 1));
    h.server
        .append(question(Address::Speaker, claude("bob"), "还在吗？"));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(inbox.messages().len(), 1, "{:?}", inbox.messages());
    h.stop().await;
}

/// The Speaker's takeover answer for alice/claude (or a provisional one,
/// which is no answer), in `q`'s thread.
fn takeover_answer(q: Seq, provisional: bool) -> NewEvent {
    let answer = NewEvent::new(
        committee(),
        Kind::Answer,
        Address::Speaker,
        "用 Idempotency-Key",
    )
    .in_thread(q)
    .on_behalf_of(claude("alice"))
    .to(claude("bob"))
    .with_meta(meta::TAKEOVER, serde_json::json!(true));
    if provisional {
        answer.with_meta(meta::PROVISIONAL, serde_json::json!(true))
    } else {
        answer
    }
}

/// While alice/claude was away the Speaker answered for it; when it comes
/// back the question is not pushed again (the hand-back note lists it). A
/// provisional answer leaves it to the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_the_speaker_answered_while_away_is_not_pushed_on_return() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    // An earlier session puts alice/claude on record; it is away now.
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s0", &wd, None).await;
    h.hook(HookKind::SessionEnd, "s0", &wd, None).await;
    h.wait_stream("refund-v2").await;
    let answered = h
        .server
        .append(question(claude("bob"), claude("alice"), "用哪个 header？"));
    h.server.append(takeover_answer(answered.seq, false));
    let open = h
        .server
        .append(question(claude("bob"), claude("alice"), "上限多少天？"));
    h.server.append(takeover_answer(open.seq, true));
    eventually("the answered question is cancelled", WAIT, || {
        let item = h.item(answered.seq.0)?;
        (item.status == ItemStatus::Cancelled).then_some(())
    })
    .await;
    eventually("the provisionally answered one still waits", WAIT, || {
        let item = h.item(open.seq.0)?;
        (item.status == ItemStatus::Pending).then_some(())
    })
    .await;

    // Alice is back: only the open question is handed to her session.
    let out = h
        .hook(HookKind::SessionStart, "s1", &wd, None)
        .await
        .expect("the backlog comes with the session start");
    let ctx = context(&out);
    assert!(pos(ctx, open.seq.0).is_some(), "{ctx}");
    assert!(pos(ctx, answered.seq.0).is_none(), "{ctx}");
    assert!(!ctx.contains("用哪个 header？"), "{ctx}");
    h.stop().await;
}

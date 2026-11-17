//! Two levels: a team's top Senate with its built-in Speaker (a scripted
//! brain) and two people, alice and bob; on alice's machine one
//! `senated --machine` with the uplink to that Senate and a Senate one
//! level down presided over by `alice/claude`, where `alice/claude/reviewer`
//! and `alice/claude/codex` sit.
//!
//! The link between the levels is alice/claude's session alone: it sits in
//! the Senate above and presides below. No event is copied between the two
//! logs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use senate_bridge::ipc::{ClientError, DocketReply, Posted, Request};
use senate_core::{
    Address, Awaiting, Event, Hub, Kind, Locale, NewEvent, SenatorName, SpeakerSeat, meta,
};

const PRESIDER: &str = "alice/claude";

fn under_team(world: &World) -> String {
    format!(
        "server_url = \"{}\"\ntoken = \"tok-alice\"\n[[preside]]\nagent = \"claude\"\n",
        world.server_url()
    )
}

async fn nested_events(desk: &Desk) -> Vec<Event> {
    let hub = desk.senate(Some(PRESIDER)).daemon.hub.clone();
    hub.read(&committee(), senate_core::Seq::ZERO, 10_000)
        .await
        .unwrap()
}

/// Alice's machine with its three sessions: the presiding Claude Code
/// session and two subagents below.
async fn alice_at_her_desk(world: &World, dir: &std::path::Path) -> (Desk, [FakeInbox; 3]) {
    let mut desk = Desk::new(dir, "alice", Locale::Zh, &under_team(world));
    desk.git_init("feature/refund-v2/alice");
    desk.start().await;
    let claude = desk.inbox("s-claude");
    let reviewer = desk.inbox("s-reviewer");
    let subcodex = desk.inbox("s-subcodex");
    let start = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&start)
            .contains("顶层 Senate 的委员会 `refund-v2`，身份是 [senator::alice/claude]"),
        "{}",
        context(&start)
    );
    assert!(
        context(&start).contains(
            "[senator::alice/claude] 是 由 [senator::alice/claude] 主持的 Senate 的 Speaker"
        ),
        "{}",
        context(&start)
    );
    let start = desk
        .session_start_as("s-reviewer", &reviewer, "claude/reviewer")
        .await
        .unwrap();
    assert!(
        context(&start).contains("由 [senator::alice/claude] 主持的 Senate 的委员会 `refund-v2`，身份是 [senator::alice/claude/reviewer]"),
        "{}",
        context(&start)
    );
    assert!(!context(&start).contains("Speaker，由这个会话主持"));
    desk.session_start_as("s-subcodex", &subcodex, "claude/codex")
        .await
        .unwrap();
    world.wait_connected("alice/claude", true).await;
    desk.wait_connected(Some(PRESIDER), "alice/claude/reviewer", true)
        .await;
    desk.wait_connected(Some(PRESIDER), "alice/claude/codex", true)
        .await;
    (desk, [claude, reviewer, subcodex])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_from_above_is_answered_after_consulting_below() {
    let world = Setup::new().without_chat().start().await;
    assert_eq!(
        me_at(&world.server_url(), "tok-alice").await.speaker,
        Some(SpeakerSeat::BuiltIn)
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, [claude, reviewer, subcodex]) = alice_at_her_desk(&world, dir.path()).await;
    let below = desk.senate(Some(PRESIDER));
    let (local_url, local_token) = (below.url.clone(), below.token.clone());
    let me = me_at(&local_url, &local_token).await;
    assert_eq!(me.presider, Some(path(PRESIDER)));
    assert_eq!(me.speaker, Some(SpeakerSeat::Senator(path(PRESIDER))));
    assert!(!me.triaged);

    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;
    world.wait_connected("bob/claude", true).await;

    // Bob's agent asks alice/claude, upstairs.
    let asked = bob
        .ask("s-bob", claude_of("alice"), "退款回调重试几次？")
        .await;
    claude.wait_for("退款回调重试几次").await;

    // alice/claude consults its subagents downstairs, as their Speaker.
    let consulted = desk
        .ask(
            "s-claude",
            senator("alice/claude/codex"),
            "回调重试的上限在代码里是多少？",
        )
        .await;
    let push = subcodex.wait_for("回调重试的上限").await;
    assert!(push.contains("来自 [speaker]"), "{push}");
    let question = nested_events(&desk)
        .await
        .into_iter()
        .find(|e| e.seq == consulted.seq)
        .unwrap();
    assert_eq!(question.from, Address::Speaker);
    desk.answer("s-subcodex", consulted.seq, "3 次，见 refund/retry.rs")
        .await;
    // The answer, put to the Speaker below, reaches the presiding session.
    let back = claude.wait_for("3 次，见 refund/retry.rs").await;
    assert!(
        back.contains("给作为 由 [senator::alice/claude] 主持的 Senate Speaker 的你"),
        "{back}"
    );

    // It answers upstairs, in its own name.
    desk.answer("s-claude", asked.seq, "重试 3 次（refund/retry.rs）")
        .await;
    bob_inbox.wait_for("重试 3 次（refund/retry.rs）").await;
    let above = world.events().await;
    let answer = above
        .iter()
        .find(|e| e.body == "重试 3 次（refund/retry.rs）")
        .unwrap();
    assert_eq!(answer.from, senator("alice/claude"));

    // Nothing of either log is in the other.
    let below_events = nested_events(&desk).await;
    for e in &below_events {
        assert!(
            !above.iter().any(|a| a.id == e.id || a.body == e.body),
            "{e:?} leaked upward"
        );
    }
    for e in &above {
        assert!(
            !below_events
                .iter()
                .any(|b| b.id == e.id || b.body == e.body),
            "{e:?} leaked downward"
        );
    }

    // The subagents cannot be addressed from above...
    let refused = bob
        .try_ask("s-bob", senator("alice/claude/reviewer"), "你好？")
        .await
        .expect_err("not reached from above");
    assert!(matches!(refused, ClientError::Remote(_)), "{refused}");
    // ...nor write above, even with alice's own token.
    let (status, _) = speak(
        &world.server_url(),
        "tok-alice",
        &NewEvent::new(
            committee(),
            Kind::Note,
            senator("alice/claude/reviewer"),
            "我在上面吗？",
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(status, 403);

    // Alice, the person, speaks in both.
    speak(
        &world.server_url(),
        "tok-alice",
        &NewEvent::new(committee(), Kind::Note, person("alice"), "上面：本周发布"),
    )
    .await
    .unwrap();
    let below_note = speak(
        &local_url,
        &local_token,
        &NewEvent::new(committee(), Kind::Note, person("alice"), "下面：先修回调"),
    )
    .await
    .unwrap();
    assert_eq!(below_note.from, person("alice"));

    // Authority: nobody but alice's presiding session writes as the Speaker
    // below, and nobody writes as the built-in Speaker above.
    let as_speaker = |body: &str| {
        NewEvent::new(committee(), Kind::Note, Address::Speaker, body)
            .with_meta(meta::BRIEF_TOP, serde_json::json!(true))
    };
    for (url, token, status) in [
        (world.server_url(), "tok-alice".to_owned(), 403),
        (world.server_url(), "tok-bob".to_owned(), 403),
        (local_url.clone(), "tok-bob".to_owned(), 401),
    ] {
        let (got, _) = speak(&url, &token, &as_speaker("x")).await.unwrap_err();
        assert_eq!(got, status, "{url} {token}");
    }
    // The subagents' sessions get no presiding tools and may not preside.
    let refused = desk
        .try_ipc::<DocketReply>(Request::Docket {
            session: Laptop::session("s-reviewer"),
            committee: None,
        })
        .await
        .unwrap_err();
    let ClientError::Remote(refused) = refused else {
        panic!("{refused}")
    };
    assert_eq!(refused.code, "not_presiding");
    let _ = reviewer;

    bob.close().await;
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_senates_keep_apart_and_survive_a_restart() {
    let world = Setup::new().without_chat().start().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, [claude, reviewer, subcodex]) = alice_at_her_desk(&world, dir.path()).await;
    let above_hub = world.hub().clone();
    let below_hub = desk.senate(Some(PRESIDER)).daemon.hub.clone();

    // Below, the subagents talk; above, nothing moves.
    let asked = desk
        .ask("s-reviewer", senator("alice/claude/codex"), "覆盖了吗？")
        .await;
    subcodex.wait_for("覆盖了吗").await;
    desk.checkpoint("s-subcodex", Some("回调测试已覆盖"), None, None)
        .await;

    // Presence: each Senate knows only its own agents.
    let presence_above = above_hub.presence(&committee()).await.unwrap();
    assert!(
        presence_above.agents.iter().all(|p| p.agent.level() == 1),
        "{presence_above:?}"
    );
    let presence_below = below_hub.presence(&committee()).await.unwrap();
    assert!(
        presence_below
            .agents
            .iter()
            .all(|p| p.agent.is_under(&path(PRESIDER)) || p.agent == path(PRESIDER)),
        "{presence_below:?}"
    );
    // Checkpoints: the subagent's is below only.
    let codex_below = path("alice/claude/codex");
    assert!(
        below_hub
            .latest_checkpoint(&committee(), &codex_below)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        above_hub
            .latest_checkpoint(&committee(), &codex_below)
            .await
            .unwrap()
            .is_none()
    );
    let head_above = above_hub.head(&committee()).await.unwrap();
    let head_below = below_hub.head(&committee()).await.unwrap();
    assert!(head_below > senate_core::Seq::ZERO);
    // Cursors: alice/claude's delivered cursor is kept per Senate, each
    // against its own log (above as a senator, below as the Speaker).
    let presider: SenatorName = path(PRESIDER);
    for (hub, head) in [(&above_hub, head_above), (&below_hub, head_below)] {
        let hub = hub.clone();
        let presider = presider.clone();
        eventually_async("the presiding session's cursor", || {
            let hub = hub.clone();
            let presider = presider.clone();
            async move {
                let seq = hub
                    .owned_cursor(&committee(), &presider, "delivered")
                    .await
                    .ok()?;
                (seq == head).then_some(())
            }
        })
        .await;
    }
    assert_ne!(head_above, head_below);
    let roster_above = above_hub.roster().await.unwrap();
    assert!(
        !roster_above
            .agents()
            .iter()
            .any(|a| a.is_under(&path(PRESIDER)))
    );

    // A restart of alice's daemon keeps both logs and seats both again.
    desk.stop().await;
    desk.start().await;
    let below_hub = desk.senate(Some(PRESIDER)).daemon.hub.clone();
    assert_eq!(below_hub.head(&committee()).await.unwrap(), head_below);
    assert!(above_hub.head(&committee()).await.unwrap() >= head_above);
    let roster_below = below_hub.roster().await.unwrap();
    for agent in ["alice/claude/reviewer", "alice/claude/codex"] {
        assert!(roster_below.knows(&path(agent)), "{agent} seated again");
    }
    // The sessions come back through their hooks; nothing is pushed twice.
    desk.prompt("s-subcodex", &subcodex, Some("claude/codex"))
        .await;
    desk.prompt("s-reviewer", &reviewer, Some("claude/reviewer"))
        .await;
    desk.prompt("s-claude", &claude, None).await;
    desk.wait_connected(Some(PRESIDER), "alice/claude/reviewer", true)
        .await;
    world.wait_connected("alice/claude", true).await;
    desk.answer("s-subcodex", asked.seq, "覆盖了，见 tests/callback.rs")
        .await;
    reviewer.wait_for("覆盖了，见 tests/callback.rs").await;
    let asked_pushes = subcodex
        .messages()
        .iter()
        .filter(|m| m.contains("覆盖了吗？"))
        .count();
    assert_eq!(asked_pushes, 1, "the question was pushed once");
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_presiding_session_away_below_still_works_and_the_person_sees_what_waits() {
    let world = Setup::new().without_chat().start().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, [claude, _reviewer, subcodex]) = alice_at_her_desk(&world, dir.path()).await;
    let below = desk.senate(Some(PRESIDER));
    let (local_url, local_token) = (below.url.clone(), below.token.clone());

    // The presiding session closes.
    desk.session_end("s-claude", &claude).await;
    world.wait_connected("alice/claude", false).await;

    // A question between subagents is delivered at once: no verdict is
    // awaited where a senator presides.
    let started = std::time::Instant::now();
    desk.ask(
        "s-reviewer",
        senator("alice/claude/codex"),
        "日志格式定了吗？",
    )
    .await;
    subcodex.wait_for("日志格式定了吗").await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "delivered in {:?}",
        started.elapsed()
    );

    // What needs the Speaker waits, and the person sees it.
    let picked = speak(
        &local_url,
        &local_token,
        &NewEvent::new(committee(), Kind::Question, person("alice"), "谁来看重试？")
            .to(agent_of("alice")),
    )
    .await
    .unwrap();
    assert_eq!(picked.to, [Address::Speaker]);
    let waiting: DocketReply = desk
        .ipc(Request::Docket {
            session: senate_bridge::ipc::SessionSelector::default(),
            committee: None,
        })
        .await;
    assert_eq!(waiting.speaker, None, "the person's read");
    let senate = waiting
        .senates
        .iter()
        .find(|s| s.senate == Some(path(PRESIDER)))
        .unwrap();
    let item = senate.pages[0]
        .items
        .iter()
        .find(|i| i.event.seq == picked.seq)
        .unwrap();
    assert!(matches!(item.awaiting, Awaiting::Pick { .. }));
    let shown = senate_bridge::preside::render_docket(&waiting, Locale::Zh);
    assert!(shown.contains("谁来看重试？"), "{shown}");

    // When the presiding session is back, it is told.
    let back = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&back).contains("现在有 1 件事等你处理"),
        "{}",
        context(&back)
    );
    let routed: Posted = desk
        .ipc(Request::Route {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: picked.seq,
            to: path("alice/claude/codex"),
            id: None,
        })
        .await;
    subcodex.wait_for("谁来看重试？").await;
    assert!(routed.seq > picked.seq);
    desk.stop().await;
    world.stop().await;
}

fn claude_of(person: &str) -> Address {
    senator(&format!("{person}/claude"))
}

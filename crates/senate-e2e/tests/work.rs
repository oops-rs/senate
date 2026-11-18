//! A developer leaving does not stall the committee: the Speaker does
//! coding work from an agent's checkpoint on request, only for people who
//! develop on the committee, keeps the work of a run that fails, and hands
//! one agent's work to another and back.
//!
//! The Speaker's hands are `senate_work`'s fakes: a [`FakeRunner`] that
//! plays scripted runs and a [`FakeForge`] that "opens" PRs at
//! `https://github.com/<repo>/pull/<n>`. Everything else is real. Alice's
//! checkpoint is a real hand-off: her checkout's `origin` is GitHub's
//! [`REPO`], with pushes redirected to a local bare repository, so the
//! checkpoint is pushed and names the GitHub repository.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::time::Duration;

use common::*;
use senate_bridge::client::ApiClient;
use senate_core::{
    Address, BranchNaming, CheckpointPayload, Event, Kind, NewEvent, PresencePayload,
    PresenceState, StatusState, WorkRun, WorkState, is_work_root,
};
use senate_speaker::{Intent, RelayKind};
use senate_work::RepoAllowlist;
use senate_work::testing::{FakeForge, FakeRun, FakeRunner};
use senated::testing::{Message, Post};

const BRANCH: &str = "feature/refund-v2/alice";
const PROGRESS: &str = "## 完成\n- 幂等键\n\n## 下一步\n- 重试路径（指数退避，上限待定）";

async fn work_world(runner: &FakeRunner, forge: &FakeForge) -> World {
    Setup::new().work(runner, forge).start().await
}

/// Alice hands off (a pushed checkpoint on her branch) and closes her
/// laptop. Returns her `HEAD` and the bare repository her pushes land in.
async fn alice_hands_off(world: &World) -> (String, std::path::PathBuf) {
    let mut alice = world.laptop("alice");
    let (head, bare) = alice.git_init_github(BRANCH);
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    let reply = alice.handoff("s-alice", PROGRESS).await;
    assert!(reply.pushed, "{reply:?}");
    assert_eq!(reply.push_error, None, "{reply:?}");
    let event = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Checkpoint)
        .expect("alice's checkpoint");
    let payload = CheckpointPayload::of(&event).expect("payload");
    assert!(payload.pushed);
    assert_eq!(
        payload.remote_url.as_deref(),
        Some(format!("git@github.com:{REPO}.git").as_str())
    );
    alice.close().await;
    (head, bare)
}

/// Bob at work: laptop open, session `s-bob` running, connected. A connected
/// agent is enough to make Bob one of the committee's developers, even
/// before it says anything there.
async fn bob_at_work(world: &World) -> (Laptop, FakeInbox) {
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected("copilot", true).await;
    (bob, inbox)
}

/// `sender` says `text` to the Speaker in the people group; the Speaker's
/// reply in that message's thread that contains `needle`.
async fn say(world: &World, sender: &str, message_ref: &str, text: &str, needle: &str) -> Post {
    world
        .say(Message::mention(GROUP, sender, message_ref, text).titled(GROUP_NAME))
        .await;
    world
        .wait_post(&format!("a reply to {text:?} with {needle:?}"), |p| {
            p.replies_to(message_ref) && p.markdown.contains(needle)
        })
        .await
}

/// 「对」 under the restatement `restated` of the message `root_ref`.
async fn confirm(world: &World, sender: &str, message_ref: &str, restated: &Post, root_ref: &str) {
    world
        .reply_to(GROUP, sender, message_ref, "对", restated, root_ref)
        .await;
}

/// The person's `chat` that says `needle`.
async fn chat_of(world: &World, needle: &str) -> Event {
    world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Chat && e.body.contains(needle))
        .expect("the chat in the log")
}

fn runs(events: &[Event]) -> Vec<WorkRun> {
    events.iter().filter_map(WorkRun::of).collect()
}

async fn terminal_run(world: &World) -> (Event, WorkRun) {
    eventually_async("the run to end", || async {
        world.events().await.into_iter().rev().find_map(|e| {
            let run = WorkRun::of(&e)?;
            run.state.is_terminal().then_some((e, run))
        })
    })
    .await
}

/// The run's card: a root in the Agent group titled `代码任务：…`.
async fn work_card(world: &World) -> Post {
    world
        .wait_post("the run's task card", |p| {
            p.is_root_in(AGENTS)
                && world
                    .fake()
                    .card(&p.message_ref)
                    .is_some_and(|c| c.title.starts_with("代码任务"))
        })
        .await
}

fn ref_of(bare: &std::path::Path, branch: &str) -> String {
    git(bare, &["rev-parse", &format!("refs/heads/{branch}")])
}

/// Work on a checkpoint branch: Bob's request in plain words is restated,
/// runs only after 「对」, shows as a task card that moves, and ends with a
/// PR on behalf of Bob, on the card and in the thread. The run is a new
/// Speaker branch from Alice's checkpoint; her branch is never the target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_on_a_checkpoint_branch() {
    let runner = FakeRunner::new().with_allowlist(RepoAllowlist::new([REPO]));
    runner.push(
        FakeRun::succeeded()
            .with_step_delay(Duration::from_millis(400))
            .with_summary("重试路径做完了：指数退避，最多 3 次。"),
    );
    let forge = FakeForge::new().with_allowlist(RepoAllowlist::new([REPO]));
    let world = work_world(&runner, &forge).await;
    world.with_agent_group().await;
    let (head, bare) = alice_hands_off(&world).await;
    let (mut bob, _inbox) = bob_at_work(&world).await;

    world.brain.interprets(Ok(Intent::Work {
        target: "alice".into(),
        instruction: "把重试路径做完".into(),
    }));
    let restated = say(
        &world,
        "u_bob",
        "m_bob_w",
        "把 alice 的重试路径做完",
        "回复「对」确认",
    )
    .await;
    let chat = chat_of(&world, "把 alice 的重试路径做完").await;
    let branch = format!("feature/refund-v2/speaker-{}", chat.seq);
    for part in [
        format!("[senator::claude] 的检查点 {BRANCH}@{}", &head[..7]),
        branch.clone(),
        "做：把重试路径做完".into(),
        "代表 [senator::bob]".into(),
    ] {
        assert!(
            restated.markdown.contains(&part),
            "{part}: {}",
            restated.markdown
        );
    }
    assert!(runner.calls().is_empty(), "nothing runs before 「对」");

    confirm(&world, "u_bob", "m_bob_ok", &restated, "m_bob_w").await;
    world
        .wait_post("the Speaker says it started", |p| {
            p.markdown.contains("好的，开始了")
        })
        .await;

    // The card, and it moves while the runner works.
    let card = work_card(&world).await;
    eventually("the card to show the run's steps while it runs", || {
        world
            .fake()
            .card_updates()
            .into_iter()
            .any(|(r, c)| {
                r == card.message_ref && c.state == StatusState::Processing && !c.steps.is_empty()
            })
            .then_some(())
    })
    .await;

    // The order: a Speaker branch from Alice's checkpoint, on behalf of Bob.
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let order = &calls[0];
    assert_eq!(order.branch(), branch);
    assert!(BranchNaming::default().is_speaker(&committee(), &order.branch()));
    assert_eq!(order.base_branch, BRANCH);
    assert_eq!(order.base_commit, head);
    assert_eq!(order.repo.slug, REPO);
    assert_eq!(order.requester, name("bob"));
    assert_eq!(order.target_agent, path("claude"));
    assert_ne!(order.branch(), order.base_branch, "never alice's branch");
    assert!(
        order
            .checkpoint_files
            .progress
            .as_deref()
            .is_some_and(|p| p.contains("重试路径"))
    );

    // Done: one PR on behalf of Bob, on the card and in both threads.
    let (done, run) = terminal_run(&world).await;
    assert_eq!(run.state, WorkState::Succeeded);
    let pr = format!("https://github.com/{REPO}/pull/1");
    assert_eq!(run.pr_url.as_deref(), Some(pr.as_str()));
    let opened = forge.opened();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].head, branch);
    assert!(
        opened[0]
            .body
            .contains("由 Senate 的 Speaker 代 [senator::bob]"),
        "{}",
        opened[0].body
    );
    assert!(!opened[0].draft);
    let root = world
        .events()
        .await
        .into_iter()
        .find(is_work_root)
        .expect("the run's root");
    assert_eq!(done.thread, Some(root.seq));
    assert_eq!(root.on_behalf_of, Some(person("bob")));
    eventually("the PR link on the card", || {
        let c = world.fake().card(&card.message_ref)?;
        c.links.iter().any(|l| l.url == pr).then_some(())
    })
    .await;
    world
        .wait_post("the PR link under the card", |p| {
            p.replies_to(&card.message_ref) && p.markdown.contains(&pr)
        })
        .await;
    world
        .wait_post("the PR link in Bob's thread", |p| {
            p.replies_to("m_bob_w") && p.markdown.contains(&pr)
        })
        .await;

    // Alice's branch is where she left it.
    assert_eq!(ref_of(&bare, BRANCH), head);
    bob.close().await;
    world.stop().await;
}

/// Only people, only developers: an agent's `/work` is refused because only
/// people may ask, a person who is not a developer of the committee is
/// refused too, and nothing runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_people_only_developers() {
    let runner = FakeRunner::new();
    let forge = FakeForge::new();
    let world = work_world(&runner, &forge).await;
    world.with_agent_group().await;
    alice_hands_off(&world).await;
    let (mut bob, inbox) = bob_at_work(&world).await;

    // A person in the group who is not a developer of the committee.
    let reply = say(
        &world,
        "u_pm",
        "m_pm_w",
        "/work alice 把重试路径做完",
        "只有这个委员会的开发",
    )
    .await;
    assert!(!reply.markdown.contains("回复「对」确认"));

    // The same words from Bob's agent, to the Speaker. The bridge's tools
    // never address the Speaker, so this is the server API with Bob's
    // token, as any tool holding it could send.
    let api = ApiClient::new(&world.server_url(), "tok-bob").unwrap();
    let said = api
        .append(
            &NewEvent::new(
                committee(),
                Kind::Note,
                senator(BOB_CLAUDE),
                "/work alice 把重试路径做完",
            )
            .to(Address::Speaker),
        )
        .await
        .unwrap();
    let asked = said.event;
    let push = inbox.wait_for("我做不了这个").await;
    assert!(push.contains("只有人可以"), "{push}");
    let refusal = world
        .events()
        .await
        .into_iter()
        .find(|e| e.from == Address::Speaker && e.body.starts_with("我做不了这个"))
        .expect("the refusal in the log");
    assert_eq!(refusal.thread, Some(asked.seq));
    assert_eq!(refusal.to, vec![senator(BOB_CLAUDE)]);

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(runner.calls().is_empty(), "nothing ran");
    assert!(forge.opened().is_empty());
    assert!(runs(&world.events().await).is_empty(), "no run in the log");
    bob.close().await;
    world.stop().await;
}

/// A failed run keeps its work: a run that hits its time cap pushes what it
/// has (a draft PR), says so with numbered next steps, and its card waits
/// for a person.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_run_keeps_its_work() {
    let runner = FakeRunner::new();
    runner.push(FakeRun::timed_out().with_pushed_commit(Some("b0a7c11")));
    let forge = FakeForge::new();
    let world = work_world(&runner, &forge).await;
    world.with_agent_group().await;
    alice_hands_off(&world).await;
    let (mut bob, _inbox) = bob_at_work(&world).await;

    let restated = say(
        &world,
        "u_bob",
        "m_bob_w",
        "/work alice 把重试路径做完",
        "回复「对」确认",
    )
    .await;
    confirm(&world, "u_bob", "m_bob_ok", &restated, "m_bob_w").await;
    let card = work_card(&world).await;

    let (done, run) = terminal_run(&world).await;
    assert_eq!(run.state, WorkState::TimedOut);
    assert_eq!(run.pushed_commit.as_deref(), Some("b0a7c11"));
    assert!(run.next_steps.len() >= 2, "{run:?}");
    let opened = forge.opened();
    assert_eq!(opened.len(), 1, "the partial branch is delivered");
    assert!(opened[0].draft, "as a draft PR");
    assert!(BranchNaming::default().is_speaker(&committee(), &opened[0].head));
    assert!(done.body.contains("时间上限"), "{}", done.body);

    let said = world
        .wait_post("the outcome in Bob's thread", |p| {
            p.replies_to("m_bob_w") && p.markdown.contains("时间上限")
        })
        .await;
    for part in ["1. ", "\n2. ", &format!("github.com/{REPO}/pull/1")] {
        assert!(said.markdown.contains(part), "{part}: {}", said.markdown);
    }
    eventually("the card to wait for a person", || {
        let c = world.fake().card(&card.message_ref)?;
        (c.state == StatusState::Suspended).then_some(())
    })
    .await;
    bob.close().await;
    world.stop().await;
}

fn presence_of(events: &[Event], agent: &str) -> Vec<PresencePayload> {
    let agent = path(agent);
    events
        .iter()
        .filter_map(PresencePayload::of)
        .filter(|p| p.agent == agent)
        .collect()
}

/// The PM asks Alice's agent something in the people group (relayed by the
/// Speaker as a question); Bob's push of it.
async fn ask_alice(world: &World, inbox: &FakeInbox, message_ref: &str, body: &str) -> String {
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: body.into(),
    }));
    world
        .say(
            Message::mention(
                GROUP,
                "u_pm",
                message_ref,
                &format!("问一下 alice 的 agent，{body}"),
            )
            .titled(GROUP_NAME),
        )
        .await;
    inbox.wait_for(body).await
}

/// Reassignment: `/reassign alice bob` and 「对」 hand Bob's session Alice's
/// checkpoint; questions to her agent reach Bob's session (also after his
/// bridge restarts, from the server's presence), his answer is mirrored as
/// Bob covering for Alice, and undo hands the work back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reassignment() {
    let world = work_world(&FakeRunner::new(), &FakeForge::new()).await;
    world.with_agent_group().await;
    let (head, _) = alice_hands_off(&world).await;
    let (mut bob, inbox) = bob_at_work(&world).await;

    let restated = say(
        &world,
        "u_pm",
        "m_pm_r",
        "/reassign alice bob",
        "回复「对」确认",
    )
    .await;
    assert!(
        restated
            .markdown
            .contains("把 [senator::claude] 在 refund-v2 的工作交给 [senator::copilot]"),
        "{}",
        restated.markdown
    );
    confirm(&world, "u_pm", "m_pm_r_ok", &restated, "m_pm_r").await;

    // Bob's session gets Alice's checkpoint and where to start.
    let note = inbox.wait_for("[senator::claude] 的工作交给你了").await;
    for part in [BRANCH, head.as_str(), "git switch -c", "重试路径"] {
        assert!(note.contains(part), "{part}: {note}");
    }
    let moved = presence_of(&world.events().await, "claude");
    let last = moved.last().expect("alice's presence");
    assert_eq!(last.state, PresenceState::Reassigned);
    assert_eq!(last.new_owner, Some(path("copilot")));
    world
        .wait_post("the line in the Agent group", |p| {
            p.is_root_in(AGENTS)
                && p.markdown
                    .contains("[senator::claude] 的工作已交给 [senator::copilot]")
        })
        .await;

    // A question to Alice's agent reaches Bob, marked as hers.
    let body = "重试上限是几次？";
    let push = ask_alice(&world, &inbox, "m_pm_q1", body).await;
    assert!(
        push.contains("给 [senator::claude] 的消息（你已接手）"),
        "{push}"
    );
    let thread = thread_of(&push, body);
    bob.answer("s-bob", thread, "三次，指数退避").await;
    let mirrored = world
        .wait_post("Bob's answer in the PM's thread", |p| {
            p.replies_to("m_pm_q1") && p.markdown.contains("三次，指数退避")
        })
        .await;
    assert!(
        mirrored
            .markdown
            .contains("[senator::copilot]（接手 [senator::claude]）"),
        "{}",
        mirrored.markdown
    );
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Answer && e.from == Address::Speaker),
        "the Speaker does not answer while Bob covers"
    );

    // Bob's bridge restarts: it learns whose work is his from the server's
    // presence (its stream resumes after the reassignment).
    bob.close().await;
    bob.open().await;
    world.wait_connected("copilot", true).await;
    let body = "退避的基数是多少？";
    let push = ask_alice(&world, &inbox, "m_pm_q2", body).await;
    assert!(
        push.contains("给 [senator::claude] 的消息（你已接手）"),
        "{push}"
    );

    // Undo: the work goes back to Alice; Bob is told.
    let restated = say(
        &world,
        "u_pm",
        "m_pm_u",
        "/reassign bob alice",
        "回复「对」确认",
    )
    .await;
    assert!(
        restated.markdown.contains("交还给 [senator::claude]"),
        "{}",
        restated.markdown
    );
    confirm(&world, "u_pm", "m_pm_u_ok", &restated, "m_pm_u").await;
    inbox.wait_for("不再推给你").await;
    let moved = presence_of(&world.events().await, "claude");
    assert_eq!(moved.last().map(|p| p.state), Some(PresenceState::Online));
    world
        .wait_post("the hand-back line in the Agent group", |p| {
            p.is_root_in(AGENTS)
                && p.markdown
                    .contains("[senator::claude] 的工作已交还给 [senator::claude]")
        })
        .await;

    // A question to Alice now is hers alone.
    let body = "回调签名用哪个密钥？";
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: body.into(),
    }));
    world
        .say(
            Message::mention(
                GROUP,
                "u_pm",
                "m_pm_q3",
                &format!("问一下 alice 的 agent，{body}"),
            )
            .titled(GROUP_NAME),
        )
        .await;
    eventually_async("the third question in the log", || async {
        world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Question && e.body.contains(body))
            .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !inbox.messages().iter().any(|m| m.contains(body)),
        "no longer pushed to Bob"
    );
    bob.close().await;
    world.stop().await;
}

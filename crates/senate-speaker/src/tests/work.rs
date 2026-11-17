//! Work on request (restate, run, PR on the card, failure next steps,
//! refusals, restart mid-run) and reassignment. Both start from, and move
//! work between, concrete agents: a person with several is asked which.

use std::sync::Arc;
use std::time::Duration;

use senate_core::{
    Address, BranchNaming, CheckpointFiles, CheckpointPayload, CheckpointTrigger, Event, Hub, Kind,
    Locale, NewEvent, PresencePayload, PresenceState, Principal, Roster, SenatorName, Seq,
    StatusPayload, StatusState, WorkRun, WorkState, meta,
};
use senate_work::testing::{FakeForge, FakeRun, FakeRunner};
use senate_work::{RepoAllowlist, RunProgress};
use tokio::sync::mpsc;

use super::continuity::{AGENTS, agents_hub};
use super::hub::MemoryHub;
use super::rig::{
    Rig, STRANGER, agent, agent_named, agent_path, answer, assert_senate_vocabulary, config_in,
    in_thread, mention, name, path, question, refund, roster, runner_of, user,
};
use super::surface::{Caps, FakeSurface};
use crate::{Intent, ScriptedBrain, SpeakerArgs, SpeakerConfig, WorkConfig, render};

const REPO: &str = "acme/refund";
const COMMIT: &str = "a41f9c2e0000000000000000000000000000beef";
/// The commit of alice's second agent's checkpoint.
const CODEX_COMMIT: &str = "c0dec0de0000000000000000000000000000cafe";

const ZH: Locale = Locale::Zh;

fn work_config() -> SpeakerConfig {
    work_config_in(Locale::Zh)
}

fn work_config_in(locale: Locale) -> SpeakerConfig {
    let mut config = config_in(locale);
    config.work = Some(WorkConfig::new(RepoAllowlist::new([REPO])));
    config
}

fn checkpoint(handle: &str, pushed: bool, remote: Option<&str>) -> NewEvent {
    checkpoint_at(&agent_path(handle), COMMIT, pushed, remote)
}

/// A hand-off checkpoint of the agent `who` on its person's branch.
fn checkpoint_at(who: &SenatorName, commit: &str, pushed: bool, remote: Option<&str>) -> NewEvent {
    let payload = CheckpointPayload {
        branch: format!("feature/refund-v2/{}", runner_of(who)),
        commit: commit.into(),
        pushed,
        remote_url: remote.map(str::to_owned),
        files: CheckpointFiles {
            progress: Some("- 完成：幂等键\n- 下一步：重试路径".into()),
            decisions: None,
            open: Some("- 重试上限是几次？".into()),
        },
        session_transcript: false,
        trigger: CheckpointTrigger::Handoff,
    };
    NewEvent::new(
        refund(),
        Kind::Checkpoint,
        Address::senator(who),
        "幂等键做完了",
    )
    .with_meta(meta::CHECKPOINT, payload.value())
}

fn github() -> String {
    format!("git@github.com:{REPO}.git")
}

/// An Agent group; alice's pushed checkpoint; bob's agent active (so bob is
/// a developer); pm is a member but not a developer.
async fn work_hub() -> Arc<MemoryHub> {
    let hub = agents_hub().await;
    for event in [
        checkpoint("alice", true, Some(&github())),
        NewEvent::new(refund(), Kind::Note, agent("bob"), "在看退款"),
    ] {
        hub.append(event).await.expect("append");
    }
    hub
}

/// [`work_hub`] with alice's second agent `codex` seated (not
/// connected), with a pushed checkpoint of its own.
async fn two_agents_hub() -> Arc<MemoryHub> {
    let hub = work_hub().await;
    let codex = path("codex");
    hub.seat(&codex, "alice");
    hub.append(checkpoint_at(&codex, CODEX_COMMIT, true, Some(&github())))
        .await
        .expect("append");
    hub
}

/// The rig's roster with carol, a person with no agent.
fn roster_with_carol() -> Roster {
    let base = roster();
    let mut principals = base.principals().to_vec();
    principals.push(Principal::new(name("carol")));
    Roster::new(principals, base.seated().to_vec()).expect("roster")
}

async fn start(hub: Arc<MemoryHub>, runner: Option<&FakeRunner>, forge: Option<&FakeForge>) -> Rig {
    start_with(hub, runner, forge, work_config()).await
}

async fn start_with(
    hub: Arc<MemoryHub>,
    runner: Option<&FakeRunner>,
    forge: Option<&FakeForge>,
    config: SpeakerConfig,
) -> Rig {
    let surface = FakeSurface::with(Caps::ADOPTED);
    let brain = ScriptedBrain::new();
    let (unbound, rx) = mpsc::channel(16);
    let mut args = SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config, rx);
    if let Some(runner) = runner {
        args = args.with_work(
            Arc::new(runner.clone()),
            forge.map(|f| Arc::new(f.clone()) as Arc<dyn senate_work::Forge>),
        );
    }
    Rig::spawn(hub, surface, brain, args, unbound).await
}

/// Say `text` to the Speaker as `who`; the reply posted under it.
async fn ask(rig: &Rig, who: &str, text: &str, message_ref: &str) -> (Event, String) {
    let chat = rig.append(mention(who, text, message_ref)).await;
    rig.settle().await;
    let reply = rig
        .surface
        .posts()
        .into_iter()
        .rev()
        .find(|p| {
            p.target
                == senate_core::PostTarget::Reply {
                    message_ref: message_ref.into(),
                    in_thread: true,
                }
        })
        .map(|p| p.markdown)
        .unwrap_or_default();
    (chat, reply)
}

/// The Speaker's latest restatement, in either language.
fn restatement(rig: &Rig) -> super::surface::Post {
    rig.surface
        .posts()
        .into_iter()
        .rev()
        .find(|p| p.markdown.contains("回复「对」确认") || p.markdown.contains("Reply \"yes\""))
        .expect("a restatement")
}

/// `said` under the Speaker's latest restatement (asked in `chat`), without
/// waiting for what follows.
async fn reply_to_restatement(
    rig: &Rig,
    who: &str,
    said: &str,
    chat: &Event,
    message_ref: &str,
) -> Event {
    let restated = restatement(rig);
    rig.append(in_thread(
        who,
        said,
        message_ref,
        chat.seq,
        restated.thread_ref.clone(),
        &restated.message_ref,
    ))
    .await
}

/// 「对」 under the restatement of `chat`, without waiting for the run.
async fn confirm_nowait(rig: &Rig, who: &str, chat: &Event, message_ref: &str) -> Event {
    reply_to_restatement(rig, who, "对", chat, message_ref).await
}

async fn confirm(rig: &Rig, who: &str, chat: &Event, message_ref: &str) -> Event {
    let yes = confirm_nowait(rig, who, chat, message_ref).await;
    rig.settle().await;
    yes
}

/// `said` (a number, a name, a path) under the Speaker's "which agent?"
/// question, asked in `chat`; the reply the Speaker posts next.
async fn choose(rig: &Rig, who: &str, said: &str, chat: &Event, message_ref: &str) -> String {
    let asked = rig
        .surface
        .posts()
        .into_iter()
        .rev()
        .find(|p| p.markdown.contains("你指的是哪一个") || p.markdown.contains("Which one"))
        .expect("the Speaker asked which agent");
    let before = rig.surface.posts().len();
    rig.append(in_thread(
        who,
        said,
        message_ref,
        chat.seq,
        asked.thread_ref.clone(),
        &asked.message_ref,
    ))
    .await;
    rig.settle().await;
    rig.surface
        .posts()
        .get(before..)
        .and_then(|new| new.last())
        .map(|p| p.markdown.clone())
        .unwrap_or_default()
}

fn runs(events: &[Event]) -> Vec<(Seq, WorkRun)> {
    events
        .iter()
        .filter_map(|e| WorkRun::of(e).map(|r| (e.seq, r)))
        .collect()
}

fn root_of(events: &[Event]) -> Event {
    events
        .iter()
        .find(|e| senate_core::is_work_root(e))
        .cloned()
        .expect("a run's root")
}

fn terminal(events: &[Event]) -> Option<(Event, WorkRun)> {
    events.iter().rev().find_map(|e| {
        let run = WorkRun::of(e)?;
        run.state.is_terminal().then(|| (e.clone(), run))
    })
}

async fn after_debounce(rig: &Rig) {
    rig.settle().await;
    tokio::time::advance(Duration::from_millis(2_100)).await;
    rig.settle().await;
}

// ----- a run, end to end ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_confirmed_run_opens_a_pr_on_behalf_of_the_asker_and_puts_it_on_the_card() {
    let runner = FakeRunner::new();
    let forge = FakeForge::new();
    let rig = start(work_hub().await, Some(&runner), Some(&forge)).await;
    let (chat, restated) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    let branch = format!("feature/refund-v2/speaker-{}", chat.seq);
    for part in [
        "[senator::claude] 的检查点 feature/refund-v2/alice@a41f9c2",
        branch.as_str(),
        "做：把重试路径做完",
        "代表 [senator::bob]",
    ] {
        assert!(restated.contains(part), "{part}: {restated}");
    }
    assert!(runner.calls().is_empty(), "nothing runs before 「对」");
    assert!(runs(&rig.hub.events(&refund())).is_empty());

    confirm(&rig, "bob", &chat, "m_w2").await;
    let events = rig.hub.events(&refund());
    let root = root_of(&events);
    assert_eq!(root.from, Address::Speaker);
    assert_eq!(root.on_behalf_of, Some(user("bob")));
    assert!(root.to.is_empty(), "nobody's session is pushed the root");
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let order = &calls[0];
    assert_eq!(order.branch(), branch);
    assert_eq!(order.base_branch, "feature/refund-v2/alice");
    assert_eq!(order.base_commit, COMMIT);
    assert_eq!(order.repo.slug, REPO);
    assert_eq!(order.requester, name("bob"));
    assert_eq!(order.target_agent, agent_path("alice"));
    assert_eq!(order.on_behalf_of, "Bob");
    assert_eq!(order.locale, ZH);
    assert!(
        order
            .checkpoint_files
            .progress
            .as_deref()
            .is_some_and(|p| p.contains("重试路径"))
    );
    let opened = forge.opened();
    assert_eq!(opened.len(), 1);
    assert!(
        opened[0].body.contains("代 [senator::bob]（Bob）"),
        "{}",
        opened[0].body
    );
    assert_eq!(opened[0].head, branch);
    assert!(!opened[0].draft);

    let (done, run) = terminal(&events).expect("a terminal status");
    assert_eq!(done.thread, Some(root.seq));
    assert_eq!(run.state, WorkState::Succeeded);
    assert_eq!(run.requester, name("bob"));
    assert_eq!(run.target_agent, agent_path("alice"));
    let pr = format!("https://github.com/{REPO}/pull/1");
    assert_eq!(run.pr_url.as_deref(), Some(pr.as_str()));
    let status = StatusPayload::of(&done).expect("status");
    assert_eq!(status.state, StatusState::Active);
    let links = status.links.expect("links");
    assert_eq!(links[0].url, pr);
    assert!(
        links
            .iter()
            .any(|l| l.url.ends_with(&format!("/tree/{branch}")))
    );

    // The link in the thread under the card, and in the asker's thread.
    let texts = rig.surface.texts();
    assert_eq!(
        texts.iter().filter(|t| t.contains(&pr)).count(),
        2,
        "{texts:#?}"
    );
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("代码任务完成（代表 [senator::bob]）：把重试路径做完")),
        "{texts:#?}"
    );
    for text in &texts {
        assert_senate_vocabulary(text);
    }
    let card = rig.surface.card("代码任务：把重试路径做完").expect("card");
    assert_eq!(
        card.target,
        senate_core::PostTarget::NewRoot {
            group_id: AGENTS.into()
        }
    );
    assert!(
        card.first
            .markdown
            .contains("**代码任务** [senator::bob]（经 Speaker）"),
        "{}",
        card.first.markdown
    );
    assert_senate_vocabulary(&card.first.markdown);
    after_debounce(&rig).await;
    let card = rig.surface.card("代码任务：把重试路径做完").expect("card");
    assert_eq!(card.now.state, StatusState::Active);
    assert_eq!(card.now.links[0].url, pr);
    assert_eq!(card.now.steps.len(), 2, "{:?}", card.now.steps);

    // A replay from before the root changes nothing.
    let hub = rig.hub.clone();
    rig.stop().await;
    hub.set_cursor(&refund(), crate::CURSOR, Seq(root.seq.0 - 1))
        .await
        .expect("rewind");
    let again = start(hub.clone(), Some(&runner), Some(&forge)).await;
    assert_eq!(runner.calls().len(), 1, "never run twice");
    assert_eq!(forge.opened().len(), 1);
    assert_eq!(
        terminal(&hub.events(&refund())).expect("done").0.seq,
        done.seq
    );
    assert_eq!(
        again.surface.posts(),
        Vec::new(),
        "every post is already recorded"
    );
    again.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_configured_branch_prefix_names_the_runs_branch() {
    let runner = FakeRunner::new();
    let forge = FakeForge::new();
    let mut config = work_config();
    if let Some(work) = config.work.as_mut() {
        work.branches = BranchNaming::new("team");
    }
    let rig = start_with(work_hub().await, Some(&runner), Some(&forge), config).await;
    let (chat, restated) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    let branch = format!("team/refund-v2/speaker-{}", chat.seq);
    assert!(restated.contains(&branch), "{restated}");
    confirm(&rig, "bob", &chat, "m_w2").await;
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].branch(), branch);
    assert_eq!(calls[0].branches, BranchNaming::new("team"));
    assert_eq!(forge.opened()[0].head, branch);
    let (_, run) = terminal(&rig.hub.events(&refund())).expect("terminal");
    assert_eq!(run.branch, branch);
}

#[tokio::test(start_paused = true)]
async fn a_failed_run_keeps_its_branch_and_offers_numbered_next_steps() {
    let runner = FakeRunner::new();
    runner.push(FakeRun::timed_out());
    let forge = FakeForge::new();
    let rig = start(work_hub().await, Some(&runner), Some(&forge)).await;
    let (chat, _) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    confirm(&rig, "bob", &chat, "m_w2").await;
    let (done, run) = terminal(&rig.hub.events(&refund())).expect("terminal");
    assert_eq!(run.state, WorkState::TimedOut);
    assert_eq!(run.pushed_commit.as_deref(), Some("fake-commit"));
    assert_eq!(run.next_steps.len(), 3);
    let status = StatusPayload::of(&done).expect("status");
    assert_eq!(status.state, StatusState::Suspended);
    assert_eq!(status.escalated_to, Some(name("bob")));
    // The partial work is a draft PR; the next steps are numbered.
    assert!(forge.opened()[0].draft);
    assert!(done.body.contains("时间上限"), "{}", done.body);
    assert!(done.body.contains("1. 重试"), "{}", done.body);
    assert!(done.body.contains("\n3. "), "{}", done.body);
    let texts = rig.surface.texts();
    assert!(texts.iter().any(|t| t.contains("1. 重试")), "{texts:#?}");
    // Nobody is mentioned as if a person had to answer a question.
    assert!(!texts.iter().any(|t| t.contains("需要你")), "{texts:#?}");
}

#[tokio::test]
async fn a_refused_order_says_why_and_pushes_nothing() {
    let runner = FakeRunner::new().with_allowlist(RepoAllowlist::new(["someone/else"]));
    let forge = FakeForge::new();
    let rig = start(work_hub().await, Some(&runner), Some(&forge)).await;
    let (chat, _) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    confirm(&rig, "bob", &chat, "m_w2").await;
    let (done, run) = terminal(&rig.hub.events(&refund())).expect("terminal");
    assert_eq!(run.state, WorkState::Refused);
    assert!(forge.opened().is_empty());
    assert_eq!(
        StatusPayload::of(&done).expect("status").state,
        StatusState::Archived
    );
    assert!(done.body.contains("没有运行"), "{}", done.body);
    assert!(
        rig.surface.texts().iter().any(|t| t.contains("没有运行")),
        "{:#?}",
        rig.surface.texts()
    );
}

#[tokio::test]
async fn a_repo_outside_the_allowlist_is_refused_before_restating() {
    let runner = FakeRunner::new();
    let hub = agents_hub().await;
    hub.append(checkpoint(
        "alice",
        true,
        Some("git@github.com:evil/repo.git"),
    ))
    .await
    .expect("append");
    hub.append(NewEvent::new(refund(), Kind::Note, agent("bob"), "hi"))
        .await
        .expect("append");
    let rig = start(hub, Some(&runner), None).await;
    let (_, reply) = ask(&rig, "bob", "/work alice 做完", "m_w1").await;
    assert!(reply.contains("这个仓库不能用"), "{reply}");
    assert!(runner.calls().is_empty());
}

// ----- whose checkpoint ---------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_agent_named_outright_is_worked_from_its_own_checkpoint() {
    let runner = FakeRunner::new();
    let rig = start(
        two_agents_hub().await,
        Some(&runner),
        Some(&FakeForge::new()),
    )
    .await;
    let (chat, restated) = ask(&rig, "bob", "/work codex 把重试路径做完", "m_w1").await;
    assert!(
        restated.contains("从 [senator::codex] 的检查点 feature/refund-v2/alice@c0dec0d"),
        "{restated}"
    );
    assert!(rig.brain.pick_inputs().is_empty(), "named: nothing to pick");
    confirm(&rig, "bob", &chat, "m_w2").await;
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_agent, path("codex"));
    assert_eq!(calls[0].base_commit, CODEX_COMMIT);
    assert_eq!(calls[0].requester, name("bob"));
    let root = root_of(&rig.hub.events(&refund()));
    assert_eq!(WorkRun::of(&root).expect("run").target_agent, path("codex"));
}

#[tokio::test(start_paused = true)]
async fn an_unnamed_agent_of_several_is_the_connected_one() {
    let hub = two_agents_hub().await;
    hub.connect(&refund(), &path("codex"));
    let runner = FakeRunner::new();
    let rig = start(hub, Some(&runner), Some(&FakeForge::new())).await;
    let (chat, restated) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    assert!(restated.contains("[senator::codex] 的检查点"), "{restated}");
    let picks = rig.brain.pick_inputs();
    assert_eq!(picks.len(), 1);
    assert_eq!(picks[0].owner, Some(name("alice")));
    confirm(&rig, "bob", &chat, "m_w2").await;
    assert_eq!(runner.calls()[0].target_agent, path("codex"));
}

#[tokio::test(start_paused = true)]
async fn when_the_speaker_cannot_tell_it_asks_and_the_answer_carries_the_order_on() {
    let runner = FakeRunner::new();
    let rig = start(
        two_agents_hub().await,
        Some(&runner),
        Some(&FakeForge::new()),
    )
    .await;
    let (chat, asked) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    assert!(
        asked.starts_with("[senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字："),
        "{asked}"
    );
    assert!(asked.contains("\n1. [senator::claude] 不在线"), "{asked}");
    assert!(asked.contains("\n2. [senator::codex] 不在线"), "{asked}");
    assert_senate_vocabulary(&asked);
    assert!(runner.calls().is_empty());

    let restated = choose(&rig, "bob", "2", &chat, "m_w2").await;
    assert!(
        restated
            .starts_with("我理解为：从 [senator::codex] 的检查点 feature/refund-v2/alice@c0dec0d"),
        "{restated}"
    );
    assert!(restated.contains("代表 [senator::bob]"), "{restated}");
    confirm(&rig, "bob", &chat, "m_w3").await;
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].target_agent, path("codex"));
    assert_eq!(calls[0].requester, name("bob"));
}

#[tokio::test]
async fn a_person_with_no_agent_has_nothing_to_work_from() {
    let hub = work_hub().await;
    hub.set_roster(roster_with_carol());
    let runner = FakeRunner::new();
    let rig = start(hub, Some(&runner), None).await;
    let (_, reply) = ask(&rig, "bob", "/work carol 把测试补上", "m_w1").await;
    assert_eq!(reply, render::no_agent_to_work_from(ZH, &name("carol")));
    assert_eq!(
        reply,
        "[senator::carol] 在这里没有 agent，没有检查点可以接着做。"
    );
    assert!(runner.calls().is_empty());
}

// ----- only people, only developers ------------------------------------------

#[tokio::test]
async fn only_a_developer_may_ask_and_never_an_agent() {
    let runner = FakeRunner::new();
    let rig = start(work_hub().await, Some(&runner), None).await;
    // A member who is not a developer.
    let (_, reply) = ask(&rig, "pm", "/work alice 把重试路径做完", "m_w1").await;
    assert_eq!(reply, render::not_a_developer(ZH));
    // Not on the roster at all: a guest changes nothing.
    // (Replaces "only team members": a guest now gets the one guest refusal.)
    let (_, reply) = ask(&rig, STRANGER, "/work alice 把重试路径做完", "m_w2").await;
    assert_eq!(reply, render::guest_refused(ZH));
    // The same words from an agent.
    let said = rig
        .append(
            NewEvent::new(refund(), Kind::Note, agent("alice"), "/work bob 做完")
                .to(Address::Speaker),
        )
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let refusal = events
        .iter()
        .find(|e| e.thread == Some(said.seq) && e.from == Address::Speaker)
        .expect("a note back");
    assert_eq!(refusal.to, vec![agent("alice")]);
    assert!(refusal.body.starts_with("我做不了这个"), "{}", refusal.body);
    assert!(runner.calls().is_empty());
    assert!(runs(&events).is_empty());

    // A developer listed in the config, with no agent activity, may.
    let mut cfg = work_config();
    if let Some(work) = cfg.work.as_mut() {
        work.developers
            .insert("refund-v2".into(), vec!["pm".into()]);
    }
    let hub = rig.hub.clone();
    rig.stop().await;
    let listed = start_with(hub, Some(&runner), None, cfg).await;
    let (_, reply) = ask(&listed, "pm", "/work alice 把重试路径做完", "m_w3").await;
    assert!(reply.contains("回复「对」确认"), "{reply}");
    listed.stop().await;
}

#[tokio::test]
async fn no_checkpoint_or_no_runner_is_said_plainly() {
    let runner = FakeRunner::new();
    let rig = start(work_hub().await, Some(&runner), None).await;
    let (_, reply) = ask(&rig, "bob", "/work bob 把测试补上", "m_w1").await;
    assert_eq!(reply, render::no_checkpoint(ZH, &agent_path("bob")));
    let (_, reply) = ask(&rig, "bob", "/work dave 把测试补上", "m_w2").await;
    assert!(reply.contains("dave"), "{reply}");
    rig.stop().await;

    let hub = work_hub().await;
    hub.append(checkpoint("bob", false, Some(&github())))
        .await
        .expect("append");
    let rig = start(hub, Some(&runner), None).await;
    let (_, reply) = ask(&rig, "bob", "/work bob 把测试补上", "m_w3").await;
    assert!(reply.contains("还没推送"), "{reply}");
    rig.stop().await;

    let bare = start(work_hub().await, None, None).await;
    let (_, reply) = ask(&bare, "bob", "/work alice 把重试路径做完", "m_w4").await;
    assert_eq!(reply, "这台服务器没有配置代码运行环境，我没法写代码。");
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn natural_language_is_restated_too() {
    let runner = FakeRunner::new();
    let rig = start(work_hub().await, Some(&runner), None).await;
    rig.brain.interprets(Ok(Intent::Work {
        target: "Alice".into(),
        instruction: "把重试路径做完".into(),
    }));
    let (chat, reply) = ask(&rig, "bob", "把 alice 的重试路径做完", "m_w1").await;
    assert!(
        reply.starts_with("我理解为：从 [senator::claude] 的检查点"),
        "{reply}"
    );
    // Anything but 「对」 cancels.
    reply_to_restatement(&rig, "bob", "算了", &chat, "m_w2").await;
    rig.settle().await;
    assert!(runs(&rig.hub.events(&refund())).is_empty());
    assert!(runner.calls().is_empty());
}

// ----- restart safety ----------------------------------------------------------

#[tokio::test]
async fn a_run_in_flight_at_restart_is_marked_failed_and_never_rerun() {
    let runner = FakeRunner::new();
    runner.push(
        FakeRun::succeeded()
            .with_steps(vec![RunProgress::started(
                "tool-1",
                "读 .senate/progress.md",
            )])
            .with_step_delay(Duration::from_secs(3_600)),
    );
    let hub = work_hub().await;
    let rig = start(hub.clone(), Some(&runner), Some(&FakeForge::new())).await;
    let (chat, _) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    confirm_nowait(&rig, "bob", &chat, "m_w2").await;
    // Wait until it runs (the run never ends on its own).
    let mut running = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if runs(&hub.events(&refund()))
            .iter()
            .any(|(_, r)| r.state == WorkState::Running)
        {
            running = true;
            break;
        }
    }
    assert!(running, "the run started");
    rig.stop().await;

    let fresh = FakeRunner::new();
    let again = start(hub.clone(), Some(&fresh), Some(&FakeForge::new())).await;
    let (done, run) = terminal(&hub.events(&refund())).expect("terminal");
    assert_eq!(run.state, WorkState::Failed);
    assert!(
        run.summary
            .as_deref()
            .is_some_and(|s| s.contains("Speaker 重启，运行中断")),
        "{run:?}"
    );
    assert!(done.body.contains("1. 重试"), "{}", done.body);
    assert!(fresh.calls().is_empty(), "never re-run automatically");
    assert_eq!(runner.calls().len(), 1);
    // Said where the asker can see it.
    assert!(
        again
            .surface
            .texts()
            .iter()
            .any(|t| t.contains("Speaker 重启，运行中断")),
        "{:#?}",
        again.surface.texts()
    );
    // And only once, however often it restarts.
    again.stop().await;
    let third = start(hub.clone(), Some(&fresh), None).await;
    let terminals = runs(&hub.events(&refund()))
        .into_iter()
        .filter(|(_, r)| r.state.is_terminal())
        .count();
    assert_eq!(terminals, 1);
    third.stop().await;
}

#[tokio::test]
async fn one_run_per_committee_at_a_time() {
    let runner = FakeRunner::new();
    runner.push(FakeRun::succeeded().with_step_delay(Duration::from_secs(3_600)));
    let rig = start(work_hub().await, Some(&runner), None).await;
    let (chat, _) = ask(&rig, "bob", "/work alice 把重试路径做完", "m_w1").await;
    confirm_nowait(&rig, "bob", &chat, "m_w2").await;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if !runner.calls().is_empty() {
            break;
        }
    }
    rig.append(mention("bob", "/work alice 再补测试", "m_w3"))
        .await;
    let mut said = String::new();
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if let Some(p) = rig.surface.posts().into_iter().find(|p| {
            p.target
                == senate_core::PostTarget::Reply {
                    message_ref: "m_w3".into(),
                    in_thread: true,
                }
        }) {
            said = p.markdown;
            break;
        }
    }
    assert!(said.contains("已经有一个代码任务在运行"), "{said}");
    rig.stop().await;
}

// ----- English ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn in_english_the_restatement_and_the_result_read_in_english() {
    let runner = FakeRunner::new();
    let forge = FakeForge::new();
    let rig = start_with(
        work_hub().await,
        Some(&runner),
        Some(&forge),
        work_config_in(Locale::En),
    )
    .await;
    let (chat, restated) = ask(&rig, "bob", "/work alice finish the retry path", "m_w1").await;
    let branch = format!("feature/refund-v2/speaker-{}", chat.seq);
    assert_eq!(
        restated,
        format!(
            "I understand: Branch {branch} off [senator::claude]'s checkpoint feature/refund-v2/alice@a41f9c2 and do: finish the retry path; then push and open a PR, on behalf of [senator::bob]. [senator::claude]'s own branch is not touched. Reply \"yes\" to confirm."
        )
    );
    // The yes is accepted in either language.
    reply_to_restatement(&rig, "bob", "yes", &chat, "m_w2").await;
    rig.settle().await;
    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].locale, Locale::En);
    assert!(
        forge.opened()[0]
            .body
            .starts_with("Opened by the Senate Speaker for [senator::bob] (Bob)."),
        "{}",
        forge.opened()[0].body
    );
    let pr = format!("https://github.com/{REPO}/pull/1");
    let texts = rig.surface.texts();
    let result = texts
        .iter()
        .find(|t| t.starts_with("Coding task done"))
        .unwrap_or_else(|| panic!("a result post: {texts:#?}"));
    assert!(
        result.starts_with("Coding task done (for [senator::bob]): finish the retry path"),
        "{result}"
    );
    assert!(result.contains(&format!("\nPR: {pr}")), "{result}");
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("OK, started (task seq ")),
        "{texts:#?}"
    );
    for text in &texts {
        assert_senate_vocabulary(text);
    }
}

// ----- reassignment --------------------------------------------------------------

fn presence_for(events: &[Event], who: &SenatorName) -> Vec<(PresencePayload, Event)> {
    events
        .iter()
        .filter_map(|e| Some((PresencePayload::of(e)?, e.clone())))
        .filter(|(p, _)| p.agent == *who)
        .collect()
}

#[tokio::test]
async fn a_reassignment_is_confirmed_pushes_the_checkpoint_and_routes_answers() {
    let hub = work_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = start(hub.clone(), None, None).await;
    let alice = agent_path("alice");
    let (chat, restated) = ask(&rig, "pm", "/reassign alice bob", "m_r1").await;
    assert!(
        restated.contains("把 [senator::claude] 在 refund-v2 的工作交给 [senator::copilot]"),
        "{restated}"
    );
    assert!(presence_for(&hub.events(&refund()), &alice).is_empty());
    confirm(&rig, "pm", &chat, "m_r2").await;

    let events = hub.events(&refund());
    let moved = presence_for(&events, &alice);
    assert_eq!(moved.len(), 1);
    let (payload, event) = &moved[0];
    assert_eq!(payload.state, PresenceState::Reassigned);
    assert_eq!(payload.new_owner, Some(agent_path("bob")));
    assert_eq!(event.from, Address::Committee);
    assert_eq!(event.on_behalf_of, Some(user("pm")));
    // Bob's one push: alice's checkpoint and where to start.
    let notes: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Note && e.to == vec![agent("bob")])
        .collect();
    assert_eq!(notes.len(), 1, "{notes:#?}");
    let note = &notes[0].body;
    for part in [
        "[senator::claude] 的工作交给你了",
        "feature/refund-v2/alice",
        COMMIT,
        "git fetch origin && git switch -c feature/refund-v2/bob origin/feature/refund-v2/alice",
        "--- PROGRESS.md ---",
        "重试上限是几次",
    ] {
        assert!(note.contains(part), "{part}: {note}");
    }
    let texts = rig.surface.texts();
    assert!(
        texts.contains(&"[senator::claude] 的工作已交给 [senator::copilot]".to_owned()),
        "{texts:#?}"
    );

    // A question to alice: bob is there, so no answer from the Speaker at
    // once; bob answers, labelled as covering for alice.
    let q = rig
        .append(question(user("pm"), agent("alice"), "重试上限几次？"))
        .await;
    rig.settle().await;
    assert!(
        !hub.events(&refund())
            .iter()
            .any(|e| e.kind == Kind::Answer && e.from == Address::Speaker),
        "no answer for alice while bob is online"
    );
    rig.append(answer(agent("bob"), q.seq, user("pm"), "三次"))
        .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert!(
        texts
            .iter()
            .any(|t| t.starts_with("[senator::copilot]（接手 [senator::claude]） 三次")),
        "{texts:#?}"
    );

    // Undo: back to alice.
    let (chat, restated) = ask(&rig, "pm", "/reassign bob alice", "m_r3").await;
    assert!(restated.contains("交还给 [senator::claude]"), "{restated}");
    confirm(&rig, "pm", &chat, "m_r4").await;
    let events = hub.events(&refund());
    let moved = presence_for(&events, &alice);
    assert_eq!(moved.len(), 2);
    assert_eq!(moved[1].0.state, PresenceState::Online);
    assert_eq!(moved[1].0.previous, Some(PresenceState::Reassigned));
    assert!(
        rig.surface
            .texts()
            .contains(&"[senator::claude] 的工作已交还给 [senator::claude]".to_owned())
    );
    let told: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Note && e.to == vec![agent("bob")])
        .collect();
    assert_eq!(told.len(), 2);
    assert!(told[1].body.contains("不再推给你"), "{}", told[1].body);
    // Nothing more to hand back.
    let (_, reply) = ask(&rig, "pm", "/reassign alice alice", "m_r5").await;
    assert!(reply.contains("没有交给别人"), "{reply}");
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn a_question_to_a_reassigned_agent_gets_a_plain_answer_when_the_new_owner_is_away() {
    let hub = work_hub().await;
    let rig = start(hub.clone(), None, None).await;
    let (chat, _) = ask(&rig, "pm", "/reassign alice bob", "m_r1").await;
    confirm(&rig, "pm", &chat, "m_r2").await;
    rig.brain.answers(Ok("记录里写的是三次".into()));
    rig.append(question(user("pm"), agent("alice"), "重试上限几次？"))
        .await;
    rig.settle().await;
    let events = hub.events(&refund());
    let behalf: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Answer && e.from == Address::Speaker)
        .collect();
    assert_eq!(behalf.len(), 1);
    assert!(
        !render::is_takeover(behalf[0]),
        "never a takeover for reassigned work"
    );
}

#[tokio::test]
async fn work_may_move_between_two_agents_of_one_person() {
    let hub = two_agents_hub().await;
    let rig = start(hub.clone(), None, None).await;
    let (chat, restated) = ask(&rig, "pm", "/reassign claude codex", "m_r1").await;
    assert!(
        restated.contains("把 [senator::claude] 在 refund-v2 的工作交给 [senator::codex]"),
        "{restated}"
    );
    confirm(&rig, "pm", &chat, "m_r2").await;
    let events = hub.events(&refund());
    let moved = presence_for(&events, &agent_path("alice"));
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].0.state, PresenceState::Reassigned);
    assert_eq!(moved[0].0.new_owner, Some(path("codex")));
    assert_eq!(moved[0].1.on_behalf_of, Some(user("pm")));
    let codex = agent_named("alice", "codex");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == Kind::Note && e.to == vec![codex.clone()])
            .count(),
        1,
        "codex is pushed claude's checkpoint"
    );
    assert!(
        rig.surface
            .texts()
            .contains(&"[senator::claude] 的工作已交给 [senator::codex]".to_owned()),
        "{:#?}",
        rig.surface.texts()
    );
}

#[tokio::test]
async fn a_reassignment_settles_both_sides_and_asks_when_it_cannot_tell() {
    let hub = two_agents_hub().await;
    let rig = start(hub.clone(), None, None).await;
    let (chat, asked) = ask(&rig, "pm", "/reassign alice bob", "m_r1").await;
    assert!(
        asked.starts_with("[senator::alice] 有 2 个 agent，你指的是哪一个？"),
        "{asked}"
    );
    assert!(presence_for(&hub.events(&refund()), &path("codex")).is_empty());

    // pm, who asked, answers by name; bob has one agent, settled at once.
    let restated = choose(&rig, "pm", "codex", &chat, "m_r2").await;
    assert!(
        restated.contains("把 [senator::codex] 在 refund-v2 的工作交给 [senator::copilot]"),
        "{restated}"
    );
    confirm(&rig, "pm", &chat, "m_r3").await;
    let events = hub.events(&refund());
    let moved = presence_for(&events, &path("codex"));
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].0.new_owner, Some(agent_path("bob")));
    assert!(
        presence_for(&events, &agent_path("alice")).is_empty(),
        "claude's work stays with claude"
    );
}

#[tokio::test]
async fn handing_back_to_a_person_with_two_agents_picks_the_one_whose_work_is_away() {
    let hub = two_agents_hub().await;
    let rig = start(hub.clone(), None, None).await;
    let (chat, _) = ask(&rig, "pm", "/reassign codex bob", "m_r1").await;
    confirm(&rig, "pm", &chat, "m_r2").await;
    let codex = path("codex");
    assert_eq!(
        presence_for(&hub.events(&refund()), &codex)[0].0.state,
        PresenceState::Reassigned
    );

    let (chat, restated) = ask(&rig, "pm", "/reassign alice alice", "m_r3").await;
    assert!(
        restated.contains(
            "把 refund-v2 上 [senator::codex] 的工作从 [senator::copilot] 交还给 [senator::codex]"
        ),
        "{restated}"
    );
    assert!(rig.brain.pick_inputs().is_empty(), "nothing to ask");
    confirm(&rig, "pm", &chat, "m_r4").await;
    let moved = presence_for(&hub.events(&refund()), &codex);
    assert_eq!(moved.len(), 2);
    assert_eq!(moved[1].0.state, PresenceState::Online);
    assert_eq!(moved[1].0.previous, Some(PresenceState::Reassigned));
    assert!(
        presence_for(&hub.events(&refund()), &agent_path("alice")).is_empty(),
        "claude was never moved"
    );
}

#[tokio::test]
async fn a_guest_may_not_reassign() {
    let hub = work_hub().await;
    let rig = start(hub.clone(), None, None).await;
    let (_, reply) = ask(&rig, STRANGER, "/reassign alice bob", "m_r1").await;
    assert_eq!(reply, render::guest_refused(ZH));
    assert!(presence_for(&hub.events(&refund()), &agent_path("alice")).is_empty());
}

#[tokio::test]
async fn in_english_a_reassignment_and_its_hand_back_read_in_english() {
    let hub = work_hub().await;
    let rig = start_with(hub.clone(), None, None, work_config_in(Locale::En)).await;
    let (chat, restated) = ask(&rig, "pm", "/reassign alice bob", "m_r1").await;
    assert!(
        restated.starts_with(
            "I understand: Hand [senator::claude]'s work on refund-v2 to [senator::copilot]."
        ),
        "{restated}"
    );
    reply_to_restatement(&rig, "pm", "yes", &chat, "m_r2").await;
    rig.settle().await;
    let (chat, _) = ask(&rig, "pm", "/reassign alice alice", "m_r3").await;
    reply_to_restatement(&rig, "pm", "yes", &chat, "m_r4").await;
    rig.settle().await;
    let texts = rig.surface.texts();
    for line in [
        "[senator::claude]'s work is now with [senator::copilot]",
        "[senator::claude]'s work is back with [senator::claude]",
    ] {
        assert!(texts.contains(&line.to_owned()), "{line}: {texts:#?}");
    }
    for text in &texts {
        assert_senate_vocabulary(text);
    }
}

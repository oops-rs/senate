//! Senate alone on one machine: `senated --machine` hosting the machine's
//! one Senate, whose Speaker is one of the person's agents, `claude` (a
//! Claude Code session), with `codex` (a Codex session) seated beside
//! it. No team server, no built-in Speaker, no chat platform.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::fake_codex::input_text;
use common::*;
use senate_bridge::ipc::{ClientError, DocketReply, Posted, PresidesReply, Request};
use senate_bridge::session::Runtime;
use senate_core::{
    Address, Awaiting, Hub, Kind, Locale, NewEvent, PickPayload, SenatePath, Seq, SpeakerSeat,
    is_takeover,
};

/// An agent as the Speaker of a root, named in full: today's alone. (A
/// bare `speaker = "claude"`, as older homes wrote it, now makes the person
/// the Speaker and claude her secretary.)
const ALONE: &str = "[local]\nspeaker = \"senator::claude\"\n";

fn codex_pushes(codex: &common::fake_codex::FakeCodex) -> Vec<String> {
    codex
        .pushes()
        .iter()
        .map(|(_, params)| input_text(params).to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_person_chains_claude_code_and_codex_under_a_presiding_agent() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, ALONE);
    desk.git_init("senate/refund-v2/alice");
    let codex = desk.codex_app_server();
    desk.start().await;
    let top = desk.senate();
    let (url, token) = (top.url.clone(), top.token.clone());

    // The machine's Senate (`alice-desk` by default): alice alone, claude
    // in the Speaker's seat, under no parent.
    let me = me_at(&url, &token).await;
    assert_eq!(me.person.as_str(), "alice");
    assert_eq!(me.senate.as_str(), "alice-desk");
    assert_eq!(me.parent, None);
    assert_eq!(me.speaker, Some(SpeakerSeat::Senator(path("claude"))));
    assert!(!me.triaged);

    // The Claude Code session presides, and is told so. The Senate is
    // named by its path from the root.
    let senate = format!("[{}]", SenatePath::top(name("alice-desk")).unwrap());
    let claude = desk.inbox("s-claude");
    let start = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&start).contains(&format!("of the Senate {senate} as [senator::claude]")),
        "{}",
        context(&start)
    );
    assert!(
        context(&start).contains(&format!(
            "[senator::claude] is the Speaker of the Senate {senate}"
        )),
        "{}",
        context(&start)
    );
    // The Codex session sits beside it, and does not preside.
    let start = desk.codex_start("thread-codex").await.unwrap();
    assert!(context(&start).contains("[senator::codex]"));
    assert!(!context(&start).contains("is the Speaker"));
    codex.idle("thread-codex");
    desk.wait_connected("claude", true).await;
    desk.wait_connected("codex", true).await;
    let presides: PresidesReply = desk
        .ipc(Request::Presides {
            session: Laptop::session("thread-codex"),
            runtime: Runtime::Codex,
            agent_env: None,
            agent_file: None,
        })
        .await;
    assert_eq!(presides.agent, Some(path("codex")));
    assert!(presides.presiding.is_none());

    // Codex asks Claude and gets an answer, at once: no verdict is awaited.
    let asked = desk
        .ask(
            "thread-codex",
            senator("claude"),
            "Which header carries the idempotency key?",
        )
        .await;
    let push = claude.wait_for("idempotency key").await;
    assert!(push.contains("from [senator::codex]"), "{push}");
    // Claude connected first, so it holds the committee's first seat and
    // chairs it; codex, its user's own other agent, is asking it. (Before
    // a connection counted as a seat, codex's first write took the chair.)
    assert!(push.contains("(your user's own agent)"), "{push}");
    assert!(!push.contains("from your committee's chair"), "{push}");
    assert!(
        push.contains("answer it directly, without asking your user first"),
        "{push}"
    );
    assert!(push.contains("mcp__senate__answer(thread="), "{push}");
    // Nothing triages here, so no verdict note stops the chain.
    assert!(!push.contains("may need your user"), "{push}");
    desk.answer(
        "s-claude",
        asked.seq,
        "Idempotency-Key, see gateway/idem.rs",
    )
    .await;
    eventually("codex gets the answer", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("Idempotency-Key, see gateway/idem.rs"))
    })
    .await;

    // Claude holds the first seat already; setting it explicitly is a
    // choice that stands whoever sits next.
    let _: serde_json::Value = desk
        .ipc(Request::SetChair {
            session: Laptop::session("s-claude"),
            committee: Some(committee()),
            senator: path("claude"),
        })
        .await;

    // Alice, as herself, writes to "an agent" (`senator::*`): two agents
    // are on the committee, so it is put to the chair to pick.
    let picked = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Please check the refund retry limit.",
        )
        .to(any_agent()),
    )
    .await
    .unwrap();
    assert_eq!(picked.to, [Address::Chair]);
    let mut candidates = PickPayload::of(&picked).unwrap().candidates;
    candidates.sort();
    assert_eq!(candidates, [path("claude"), path("codex")]);
    let put = claude.wait_for("refund retry limit").await;
    assert!(put.contains("chair_route"), "{put}");
    // Codex may not route it: it does not chair.
    let refused = desk
        .try_ipc::<Posted>(Request::ChairRoute {
            session: Laptop::session("thread-codex"),
            seq: picked.seq,
            to: path("codex"),
            id: None,
        })
        .await
        .unwrap_err();
    let ClientError::Remote(refused) = refused else {
        panic!("{refused}")
    };
    assert_eq!(refused.code, "not_chairing");
    // The chair routes it to Codex, saying where it came from.
    let routed: Posted = desk
        .ipc(Request::ChairRoute {
            session: Laptop::session("s-claude"),
            seq: picked.seq,
            to: path("codex"),
            id: None,
        })
        .await;
    eventually("codex is relayed it", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("refund retry limit"))
    })
    .await;
    let relay = top
        .daemon
        .hub
        .get(&committee(), routed.seq)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(relay.from, senator("claude"));
    assert_eq!(relay.to, [senator("codex")]);

    // Codex leaves a checkpoint, then goes away.
    desk.checkpoint(
        "thread-codex",
        Some("retry limit: 3 attempts, see refund/retry.rs"),
        None,
        None,
    )
    .await;
    desk.codex_end("thread-codex").await;
    desk.wait_connected("codex", false).await;

    // A question for Codex while it is away: the presiding session covers
    // from Codex's own checkpoint, never by asking itself instead.
    let away = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "What is the retry limit?",
        )
        .to(senator("codex")),
    )
    .await
    .unwrap();
    let docket: DocketReply = desk
        .ipc(Request::Docket {
            session: Laptop::session("s-claude"),
            committee: Some(committee()),
        })
        .await;
    let senate = &docket.senates[0];
    let item = senate.pages[0]
        .items
        .iter()
        .find(|i| i.event.seq == away.seq)
        .unwrap();
    assert_eq!(
        item.awaiting,
        Awaiting::Away {
            agents: vec![path("codex")]
        }
    );
    let checkpoint = &senate.checkpoints[0];
    assert_eq!(checkpoint.agent, path("codex"));
    assert!(
        checkpoint
            .checkpoint
            .files
            .progress
            .as_deref()
            .unwrap()
            .contains("3 attempts")
    );
    let reroute = desk
        .try_ipc::<Posted>(Request::Route {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: away.seq,
            to: path("claude"),
            id: None,
        })
        .await;
    assert!(reroute.is_err(), "never rerouted to a sibling");
    let covered: Posted = desk
        .ipc(Request::Cover {
            session: Laptop::session("s-claude"),
            committee: committee(),
            thread: away.seq,
            body: "3 attempts (per its checkpoint).".into(),
            agent: None,
            id: None,
        })
        .await;
    let answer = top
        .daemon
        .hub
        .get(&committee(), covered.seq)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(senator("codex")));
    assert_eq!(answer.to, [person("alice")]);
    assert!(is_takeover(&answer));
    assert_eq!(
        answer.byline(Locale::En).unwrap(),
        "[speaker::codex] takeover"
    );
    assert_eq!(answer.byline(Locale::Zh).unwrap(), "[speaker::codex] 接管");
    assert_eq!(
        answer.refs.as_ref().unwrap().branch.as_deref(),
        Some("senate/refund-v2/alice")
    );
    let docket: DocketReply = desk
        .ipc(Request::Docket {
            session: Laptop::session("s-claude"),
            committee: Some(committee()),
        })
        .await;
    let left: Vec<Seq> = docket.senates[0].pages[0]
        .items
        .iter()
        .map(|i| i.event.seq)
        .collect();
    // Covered: off the docket. The relay Codex never answered before it
    // left is still there, now for covering too.
    assert_eq!(left, [routed.seq]);

    // The presiding session ends. A question to its agent, the Speaker's own
    // seat holder, is that agent's to answer when its session opens: it is
    // never on the docket to cover, and covering for it is refused.
    desk.session_end("s-claude", &claude).await;
    desk.wait_connected("claude", false).await;
    let own = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Where does the idempotency key live?",
        )
        .to(senator("claude")),
    )
    .await
    .unwrap();
    let page = top.daemon.hub.docket(&committee()).await.unwrap();
    assert!(
        page.items.iter().all(|i| i.event.seq != own.seq),
        "{:?}",
        page.items
    );
    assert!(page.items.iter().all(
        |i| !matches!(&i.awaiting, Awaiting::Away { agents } if agents.contains(&path("claude")))
    ),);
    let refused = desk
        .try_ipc::<Posted>(Request::Cover {
            session: Laptop::session("s-claude"),
            committee: committee(),
            thread: own.seq,
            body: "from nowhere".into(),
            agent: Some(path("claude")),
            id: None,
        })
        .await;
    assert!(refused.is_err(), "no covering for the seat holder");
    desk.stop().await;
}

/// What the person types: `senate ask '*' --wait`, through the very requests
/// the command sends (`senate_bridge::speak`), with the presiding session
/// routing it and the chosen agent answering; then the inbox, which has
/// nothing waiting but the settled thread, listed with `--all`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_person_asks_whichever_agent_from_the_command_line_and_waits_for_the_answer() {
    use senate_bridge::ipc::{InboxReply, SayKind, SessionSelector};
    use senate_bridge::mcp::{ShowAll, render_inbox};

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, ALONE);
    let codex = desk.codex_app_server();
    desk.start().await;
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.codex_start("thread-codex").await.unwrap();
    codex.idle("thread-codex");
    desk.wait_connected("claude", true).await;
    desk.wait_connected("codex", true).await;
    // Claude sat first, so it chairs the committee, and a chair that is an
    // agent picks for `senator::*`. This test is about the Speaker picking,
    // so the person chairs (an explicit choice, which stands).
    let _: serde_json::Value = desk
        .ipc(Request::SetChair {
            session: Laptop::session("s-claude"),
            committee: Some(committee()),
            senator: path("alice"),
        })
        .await;
    let socket = desk.paths.socket();

    // `senate ask '*' ... --wait`: two agents are on the committee, so the server
    // puts it to the presiding session to pick.
    let sent = senate_bridge::speak::send(
        &socket,
        committee(),
        SayKind::Question,
        "*",
        "Please check the refund retry limit.".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.posted.to, [Address::Speaker]);
    assert_eq!(sent.wrote.to_string(), "senator::*");
    assert!(
        sent.render(true)
            .contains("to [speaker] (you wrote [senator::*]; the Speaker picks which agent)"),
        "{}",
        sent.render(true)
    );
    let waiting = tokio::spawn({
        let (socket, sent) = (socket.clone(), sent.clone());
        async move {
            senate_bridge::speak::wait_answer(&socket, &sent, std::time::Duration::from_secs(30))
                .await
        }
    });

    // The presiding session routes it to Codex; Codex answers the relay.
    claude.wait_for("refund retry limit").await;
    let routed: Posted = desk
        .ipc(Request::Route {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: sent.posted.seq,
            to: path("codex"),
            id: None,
        })
        .await;
    eventually("codex is relayed it", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("refund retry limit"))
    })
    .await;
    assert!(!waiting.is_finished(), "nothing answered yet");
    desk.answer(
        "thread-codex",
        routed.seq,
        "3 attempts, see refund/retry.rs",
    )
    .await;

    // The waiting command returns the answer, with its byline.
    let answer = waiting.await.unwrap().unwrap().expect("an answer");
    assert_eq!(answer.from, senator("codex"));
    assert_eq!(
        senate_bridge::speak::render_answer(&answer, Locale::En),
        format!(
            "[senator::codex] answered in thread {}:\n3 attempts, see refund/retry.rs",
            routed.seq
        )
    );
    // `senate thread` reads it from the question's seq, relay included.
    let (thread, _) = senate_bridge::speak::thread(&socket, committee(), sent.posted.seq)
        .await
        .unwrap();
    let kinds: Vec<_> = thread.events.iter().map(|e| e.kind.clone()).collect();
    assert_eq!(kinds, [Kind::Question, Kind::Question, Kind::Answer]);
    assert_eq!(
        thread.events[1].byline(Locale::En).unwrap(),
        "[speaker::alice]"
    );

    // The default inbox: nothing waits, the settled thread is counted.
    let inbox = |all: bool| {
        let desk = &desk;
        async move {
            desk.ipc::<InboxReply>(Request::Inbox {
                session: SessionSelector::default(),
                committee: Some(committee()),
                agent: None,
                all,
            })
            .await
        }
    };
    let waiting = inbox(false).await;
    assert!(waiting.threads.is_empty(), "{:?}", waiting.threads);
    assert_eq!(waiting.settled, 1);
    let text = render_inbox(&waiting, Locale::En, ShowAll::Flag);
    assert!(
        text.starts_with("Nothing open waits on")
            && text.ends_with("1 answered thread(s) not shown; --all lists them."),
        "{text}"
    );
    let all = inbox(true).await;
    assert_eq!(all.settled, 0);
    assert_eq!(all.threads.len(), 1);
    assert_eq!(all.threads[0].root, routed.seq);
    assert!(all.threads[0].is_settled());
    assert!(
        render_inbox(&all, Locale::En, ShowAll::Flag).contains("settled: answered, nobody waits")
    );

    // Alice follows up: the thread waits again, on the agent that answered.
    let (posted, _) = senate_bridge::speak::reply(
        &socket,
        committee(),
        sent.posted.seq,
        "No: I meant the retry limit of the refund webhook.".into(),
    )
    .await
    .unwrap();
    assert_eq!(posted.to, [senator("codex")]);
    assert_eq!(posted.thread, Some(routed.seq));
    let waiting = inbox(false).await;
    assert_eq!(waiting.threads.len(), 1);
    assert_eq!(waiting.settled, 0);
    desk.stop().await;
}

/// The first real task run, replayed: the person asks their agent for
/// something; the agent reports (processing, with steps), answers to the
/// person, escalates to them with its open decisions, checkpoints, and ticks
/// off its steps with `active`. What the person's inbox shows at each point,
/// through the very requests and renderers `senate inbox` and `senate thread`
/// use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_agent_hands_back_stays_in_the_persons_inbox_until_they_act() {
    use senate_bridge::ipc::{InboxReply, SayKind, SessionSelector};
    use senate_bridge::mcp::ShowAll;
    use senate_bridge::person;
    use senate_core::{StatusPayload, StatusState, Step};

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, ALONE);
    desk.git_init("main");
    desk.start().await;
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    let socket = desk.paths.socket();

    let inbox = |all: bool| {
        let desk = &desk;
        async move {
            desk.ipc::<InboxReply>(Request::Inbox {
                session: SessionSelector::default(),
                committee: Some(committee()),
                agent: None,
                all,
            })
            .await
        }
    };
    let shown = |reply: &InboxReply, locale| person::render_inbox(reply, locale, ShowAll::Flag);

    // `senate request claude - --committee …`
    let sent = senate_bridge::speak::send(
        &socket,
        committee(),
        SayKind::Request,
        "claude",
        "Plan the refund retry change.".into(),
    )
    .await
    .unwrap();
    let root = sent.posted.seq;
    claude.wait_for("refund retry change").await;

    // The agent has it: it waits on the agent, nothing is news to the person.
    let mut working = StatusPayload::new(StatusState::Processing);
    working.title = Some("Retry plan".into());
    working.steps = Some(vec![Step {
        id: "read".into(),
        label: "read the code".into(),
        done: false,
    }]);
    desk.status("s-claude", root, working).await;
    let reply = inbox(false).await;
    assert!(
        reply.threads[0]
            .waits_on_person(&name("alice"), Seq(0))
            .is_none()
    );
    let text = shown(&reply, Locale::En);
    assert!(
        text.contains("open task thread(s)") && !text.contains("wait on you"),
        "{text}"
    );

    // It answers to the person: settled for agents, news for the person.
    desk.answer("s-claude", root, "Plan is in docs/retry.md\nSecond line")
        .await;
    let reply = inbox(false).await;
    assert_eq!(reply.settled, 0, "not left out as settled");
    assert!(
        reply.threads[0].is_settled(),
        "the agents' rule is unchanged"
    );
    let text = shown(&reply, Locale::En);
    assert!(
        text.starts_with("1 thread(s) on refund-v2 wait on you ([senator::alice]); 1 with news:"),
        "{text}"
    );
    assert!(
        text.contains("[senator::claude] answered: Plan is in docs/retry.md"),
        "{text}"
    );
    assert!(shown(&reply, Locale::Zh).contains("回答了：Plan is in docs/retry.md"));

    // It escalates with what it needs, leaves a checkpoint, and ticks off
    // its steps. Nothing it does after the escalation withdraws it.
    desk.escalate("s-claude", root, "1. which header?\n2. which table?")
        .await;
    desk.checkpoint("s-claude", Some("# Progress\nplanned"), None, None)
        .await;
    let mut done = StatusPayload::new(StatusState::Active);
    done.steps = Some(vec![Step {
        id: "read".into(),
        label: "read the code".into(),
        done: true,
    }]);
    desk.status("s-claude", root, done).await;
    let reply = inbox(false).await;
    let t = &reply.threads[0];
    assert_eq!(t.state, StatusState::Suspended);
    assert_eq!(t.title, "Retry plan");
    assert!(t.steps[0].done, "the progress update still landed");
    let text = shown(&reply, Locale::En);
    assert!(text.contains("new for you"), "{text}");
    assert!(
        text.contains("escalated this to you:\n      1. which header?\n      2. which table?"),
        "{text}"
    );
    assert!(!text.contains("settled"), "{text}");
    let zh = shown(&reply, Locale::Zh);
    assert!(
        zh.contains("把它转给了你：\n      1. which header?"),
        "{zh}"
    );

    // Before they have read any of it, the person's reply about something
    // else arrives: it crossed the escalation. It does not end it, the
    // command says so, and the thread is still new for them with its reason.
    let (crossed, _) = senate_bridge::speak::reply(
        &socket,
        committee(),
        root,
        "Unrelated: use the staging database.".into(),
    )
    .await
    .unwrap();
    assert!(crossed.unseen_escalation.is_some(), "{crossed:?}");
    let line = senate_bridge::speak::render_replied(root, &crossed, Locale::En);
    assert!(
        line.contains("There is something newer in thread") && line.contains("senate thread"),
        "{line}"
    );
    let reply = inbox(false).await;
    let t = &reply.threads[0];
    assert!(t.escalation.is_some(), "still in force");
    assert_eq!(t.state, StatusState::Suspended);
    assert_eq!(t.waits(), Some(senate_core::WaitsOn::Person));
    let text = shown(&reply, Locale::En);
    assert!(text.contains("new for you"), "{text}");
    assert!(
        text.contains("escalated this to you:\n      1. which header?"),
        "{text}"
    );

    // Reading it clears the news, not the wait.
    let (_, _) = senate_bridge::speak::thread(&socket, committee(), root)
        .await
        .unwrap();
    let reply = inbox(false).await;
    let text = shown(&reply, Locale::En);
    assert!(
        text.contains("0 with news") && !text.contains("new for you"),
        "{text}"
    );
    assert!(text.contains("seen; it waits until you act"), "{text}");
    assert!(text.contains("1. which header?"), "{text}");

    // Another progress update changes nothing about the wait.
    let mut more = StatusPayload::new(StatusState::Processing);
    more.title = Some("Retry plan v2".into());
    desk.status("s-claude", root, more).await;
    let reply = inbox(false).await;
    assert_eq!(reply.threads[0].title, "Retry plan v2");
    assert_eq!(reply.threads[0].state, StatusState::Suspended);
    assert!(shown(&reply, Locale::En).contains("1. which header?"));

    // The person replies: it is the agent's turn again, and nothing waits
    // on the person.
    let (answered, _) =
        senate_bridge::speak::reply(&socket, committee(), root, "Use Idempotency-Key.".into())
            .await
            .unwrap();
    assert_eq!(answered.unseen_escalation, None, "they had read it by now");
    let reply = inbox(false).await;
    assert!(reply.waits_on_person(&reply.threads[0]).is_none());
    assert_eq!(reply.threads[0].waits(), Some(senate_core::WaitsOn::Agents));
    let text = shown(&reply, Locale::En);
    assert!(!text.contains("wait on you"), "{text}");
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_codex_threads_in_one_worktree_choose_separate_seats() {
    use senate_bridge::ipc::JoinReply;

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, ALONE);
    let codex = desk.codex_app_server();
    desk.start().await;
    for id in ["author-thread", "review-thread"] {
        codex.idle(id);
        let start = desk.codex_start_with_identity(id, true).await.unwrap();
        assert!(context(&start).contains(&format!("[senator::codex-{id}]")));
    }
    desk.wait_connected("codex-author-thread", true).await;
    let named = Request::JoinAs {
        session: Laptop::session("review-thread"),
        committee: committee(),
        senator: path("reviewer"),
    };
    for _ in 0..2 {
        let reply: JoinReply = desk.ipc(named.clone()).await;
        assert_eq!(reply.agent, path("codex-review-thread"));
        assert_eq!(reply.alias, Some(path("reviewer")));
    }
    // Closing and resuming the thread across a machine restart keeps the choice.
    desk.codex_end("review-thread").await;
    desk.stop().await;
    desk.start().await;
    let start = desk
        .codex_start_with_identity("review-thread", true)
        .await
        .unwrap();
    assert!(context(&start).contains("[senator::codex-review-thread]"));
    let joined: JoinReply = desk
        .ipc(Request::Join {
            session: Laptop::session("review-thread"),
            committee: committee(),
        })
        .await;
    assert_eq!(joined.agent, path("codex-review-thread"));
    desk.wait_connected("codex-review-thread", true).await;
    desk.wait_connected("codex-author-thread", true).await;

    for (sender, to, body, intended) in [
        (
            "author-thread",
            "reviewer",
            "Review the retry policy?",
            "review-thread",
        ),
        (
            "review-thread",
            "codex-author-thread",
            "Which revision should I review?",
            "author-thread",
        ),
    ] {
        let asked = desk.ask(sender, senator(to), body).await;
        let event = desk
            .senate()
            .daemon
            .hub
            .get(&committee(), asked.seq)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.from, senator(&format!("codex-{sender}")));
        assert_eq!(event.to, [senator(&format!("codex-{intended}"))]);
        eventually("directed question pushed to its own Codex thread", || {
            let threads: Vec<_> = codex
                .pushes()
                .into_iter()
                .filter(|(_, p)| input_text(p).contains(body))
                .map(|(_, p)| p["threadId"].clone())
                .collect();
            (!threads.is_empty()).then(|| assert_eq!(threads, [serde_json::json!(intended)]))
        })
        .await;
    }
    let conflict = Request::JoinAs {
        session: Laptop::session("author-thread"),
        committee: committee(),
        senator: path("reviewer"),
    };
    assert!(desk.try_ipc::<JoinReply>(conflict.clone()).await.is_err());
    let renamed: JoinReply = desk
        .ipc(Request::JoinAs {
            session: Laptop::session("review-thread"),
            committee: committee(),
            senator: path("editor"),
        })
        .await;
    assert_eq!(renamed.agent, path("codex-review-thread"));
    let reused: JoinReply = desk.ipc(conflict).await;
    assert_eq!(reused.agent, path("codex-author-thread"));
    let senate_core::alias::KnownAliases::Scoped(aliases) = desk.ipc(Request::Aliases).await else {
        panic!("a current server keeps aliases per committee");
    };
    let flat = aliases.flat();
    assert_eq!(flat[&path("editor")], path("codex-review-thread"));
    assert_eq!(flat[&path("reviewer")], path("codex-author-thread"));
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_alias_on_two_committees_asks_which_until_one_is_removed() {
    use senate_bridge::ipc::JoinReply;
    use senate_core::alias::KnownAliases;

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, ALONE);
    let codex = desk.codex_app_server();
    desk.start().await;
    for id in ["author-thread", "review-thread"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
    }
    desk.wait_connected("codex-author-thread", true).await;
    // A second committee, known to the server once something is said there.
    let other = senate_core::CommitteeId::new("other").unwrap();
    desk.senate()
        .daemon
        .hub
        .append(NewEvent::new(
            other.clone(),
            Kind::Note,
            senator("alice"),
            "opening",
        ))
        .await
        .unwrap();
    for (id, on) in [
        ("review-thread", committee()),
        ("author-thread", other.clone()),
    ] {
        let reply: JoinReply = desk
            .ipc(Request::JoinAs {
                session: Laptop::session(id),
                committee: on,
                senator: path("main"),
            })
            .await;
        assert_eq!(reply.alias, Some(path("main")));
    }
    let KnownAliases::Scoped(aliases) = desk.ipc(Request::Aliases).await else {
        panic!("a current server keeps aliases per committee");
    };
    assert_eq!(
        aliases.get(&committee(), &path("main")),
        Some(&path("codex-review-thread"))
    );
    assert_eq!(
        aliases.get(&other, &path("main")),
        Some(&path("codex-author-thread"))
    );

    // A bare alias of two agents is not guessed: the candidates are named.
    let ambiguous = desk
        .try_ipc::<std::path::PathBuf>(Request::Attach {
            senator: path("main").into(),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(
        ambiguous.contains("other::main") && ambiguous.contains(&format!("{}::main", committee())),
        "{ambiguous}"
    );

    // `alias rm` takes one committee's alias away; the other one is then
    // the only match, from any committee.
    let _: serde_json::Value = desk
        .ipc(Request::RemoveAlias {
            primary: path("codex-author-thread"),
            alias: path("main"),
            committee: Some(other.clone()),
        })
        .await;
    let KnownAliases::Scoped(aliases) = desk.ipc(Request::Aliases).await else {
        panic!("a current server keeps aliases per committee");
    };
    assert_eq!(aliases.get(&other, &path("main")), None);
    assert_eq!(
        aliases.flat().get(&path("main")),
        Some(&path("codex-review-thread"))
    );
    desk.stop().await;
}

//! The roles on a developer machine and on a team's Senate: the boundary
//! rule (a person's message to another committee is delivered directly; an
//! agent's goes by way of the Speaker's desk, where a secretary forwards
//! it, refuses it, or leaves it for the Speaker), a committee's chair
//! picking for `senator::*`, and a principal that is not a person running
//! agents.
//!
//! On the machine: alice's Senate, alice its Speaker, `claude` her
//! secretary working the desk; `copilot` on `refund-v2` and `codex` on
//! `ibot`, all her agents.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_bridge::ipc::{Forwarded, Request, SayKind, SendReply};
use senate_core::{
    Address, CommitteeId, Hub, Kind, Locale, NewEvent, Principal, PrincipalKind, meta,
};

fn ibot() -> CommitteeId {
    CommitteeId::new("ibot").unwrap()
}

/// Wait until the desk worker's docket (its `speaker_docket` tool) lists
/// the item `seq` on ibot.
async fn on_the_desk(desk: &Desk, seq: senate_core::Seq) {
    eventually_async("the item on claude's docket", || async {
        let docket: senate_bridge::ipc::DocketReply = desk
            .ipc(Request::Docket {
                session: Laptop::session("s-claude"),
                committee: Some(ibot()),
            })
            .await;
        docket
            .senates
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.items)
            .any(|i| i.event.seq == seq)
            .then_some(())
    })
    .await;
}

/// What codex's `send` tool does, to a qualified name.
async fn send(desk: &Desk, session: &str, at: &str, body: &str) -> SendReply {
    desk.ipc(Request::Send {
        session: Laptop::session(session),
        kind: SayKind::Question,
        to: Vec::new(),
        at: Some(senate_core::Locator::parse_short(at).unwrap()),
        body: body.into(),
        refs: None,
        id: None,
    })
    .await
}

/// Alice's machine alone: claude (her secretary) and copilot on
/// refund-v2, codex on ibot.
async fn alices_machine(dir: &std::path::Path) -> (Desk, FakeInbox, FakeInbox, FakeInbox) {
    let mut desk = Desk::new(
        dir,
        "alice",
        Locale::En,
        "[local]\nsecretaries = [\"claude\"]\n",
    );
    desk.start().await;
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    let copilot = desk.inbox("s-copilot");
    desk.session_start_as("s-copilot", &copilot, "copilot")
        .await
        .unwrap();
    desk.wait_connected("copilot", true).await;
    let codex = desk.inbox("s-codex");
    desk.session_start_as("s-codex", &codex, "codex")
        .await
        .unwrap();
    // A committee exists once it has an event, and an agent joins only
    // those (refund-v2 does now: its first agent took a seat there on
    // connecting), so alice opens ibot first.
    desk.senate()
        .daemon
        .hub
        .append(NewEvent::new(
            ibot(),
            Kind::Note,
            person("alice"),
            "ibot opens",
        ))
        .await
        .unwrap();
    let _: serde_json::Value = desk
        .ipc(Request::Join {
            session: Laptop::session("s-codex"),
            committee: ibot(),
        })
        .await;
    eventually_async("codex on ibot", || async {
        desk.senate()
            .daemon
            .hub
            .is_agent_connected(&ibot(), &path("codex"))
            .await
            .then_some(())
    })
    .await;
    desk.post("s-codex", "ibot is open").await;
    (desk, claude, copilot, codex)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn across_committees_a_person_goes_directly_and_an_agent_by_way_of_the_desk() {
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, claude, copilot, codex) = alices_machine(dir.path()).await;
    let hub = desk.senate().daemon.hub.clone();
    let office = hub.office().unwrap();
    assert_eq!(office.speaker.to_string(), "senator::alice");
    assert_eq!(office.desk, [path("claude")]);

    // Alice, a person, standing on ibot: delivered on refund-v2 directly.
    let sent = senate_bridge::speak::send(
        &desk.paths.socket(),
        ibot(),
        SayKind::Question,
        "refund-v2::copilot",
        "Is the retry window configurable?".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.posted.committee, committee());
    assert_eq!(sent.posted.to, [senator("copilot")]);
    copilot.wait_for("Is the retry window configurable?").await;

    // codex, an agent, the same: put to the desk on its own committee.
    let asked = send(
        &desk,
        "s-codex",
        "refund-v2::copilot",
        "Which status code does the refund callback return?",
    )
    .await;
    assert_eq!(asked.posted.committee, ibot());
    assert_eq!(asked.posted.to, [Address::Speaker]);
    assert_eq!(
        asked.intended.as_ref().unwrap().to_string(),
        "/alice-desk::refund-v2::senator::copilot"
    );
    assert_eq!(
        asked.speaker.as_ref().unwrap().to_string(),
        "senator::alice"
    );
    // claude works the desk: its docket has it.
    on_the_desk(&desk, asked.posted.seq).await;

    // It forwards it: a new root at copilot, from the office.
    let forwarded: Forwarded = desk
        .ipc(Request::Forward {
            session: Laptop::session("s-claude"),
            committee: ibot(),
            seq: asked.posted.seq,
            id: None,
        })
        .await;
    assert_eq!(forwarded.posted.committee, committee());
    assert_eq!(
        forwarded.to.to_string(),
        "/alice-desk::refund-v2::senator::copilot"
    );
    let push = copilot.wait_for("Which status code").await;
    let root = thread_of(&push, "Which status code");
    assert_eq!(root, forwarded.posted.seq);
    let stored = hub.get(&committee(), root).await.unwrap().unwrap();
    assert_eq!(stored.from, Address::Speaker);
    assert_eq!(stored.meta[meta::ACTED_BY], serde_json::json!("claude"));
    // copilot answers there; the answer comes back to codex's thread.
    desk.answer("s-copilot", root, "202, with a retry header")
        .await;
    let back = codex.wait_for("202, with a retry header").await;
    assert!(back.contains("202"), "{back}");
    let relayed = eventually_async("the answer in codex's thread", || async {
        desk.events_of(&ibot())
            .await
            .into_iter()
            .find(|e| e.body == "202, with a retry header")
    })
    .await;
    assert_eq!(relayed.thread, Some(asked.posted.seq));
    assert_eq!(relayed.from, Address::Speaker);
    assert_eq!(
        relayed.meta[meta::ANSWERED_BY],
        serde_json::json!("/alice-desk::refund-v2::senator::copilot")
    );

    // Another is refused, with its reason, in codex's thread.
    let again = send(&desk, "s-codex", "refund-v2::copilot", "Can you deploy it?").await;
    on_the_desk(&desk, again.posted.seq).await;
    let _: senate_bridge::ipc::Posted = desk
        .ipc(Request::Refuse {
            session: Laptop::session("s-claude"),
            committee: ibot(),
            seq: again.posted.seq,
            reason: "Deploys are alice's call; ask her.".into(),
            id: None,
        })
        .await;
    codex.wait_for("Deploys are alice's call; ask her.").await;
    let refusal = desk
        .events_of(&ibot())
        .await
        .into_iter()
        .find(|e| e.body == "Deploys are alice's call; ask her.")
        .unwrap();
    assert_eq!(refusal.kind, Kind::Answer);
    assert_eq!(refusal.thread, Some(again.posted.seq));
    assert_eq!(refusal.to, [senator("codex")]);

    // claude goes away: no one works the desk, and the next one waits on
    // alice, in her inbox.
    desk.session_end("s-claude", &claude).await;
    let waiting = send(
        &desk,
        "s-codex",
        "refund-v2::copilot",
        "Who owns the refund queue?",
    )
    .await;
    // What `senate inbox` shows her: the desk, as the Speaker.
    let docket = eventually_async("the desk in alice's inbox", || async {
        let docket: senate_bridge::ipc::DocketReply = desk
            .ipc(Request::Docket {
                session: Default::default(),
                committee: None,
            })
            .await;
        docket
            .senates
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.items)
            .any(|i| i.event.seq == waiting.posted.seq)
            .then_some(docket)
    })
    .await;
    let text = senate_bridge::preside::render_docket(&docket, Locale::En);
    assert!(text.contains("Who owns the refund queue?"), "{text}");
    let status = desk.machine_status().await;
    let roles = status.roles.unwrap();
    assert!(roles.present.is_empty(), "{:?}", roles.present);
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_speaker_of_a_root_still_presides_with_and_without_a_secretary() {
    // `speaker = "senator::claude"`: today's alone, named in full.
    for secretaries in ["", "secretaries = [\"copilot\"]\n"] {
        let dir = tempfile::tempdir().unwrap();
        let mut desk = Desk::new(
            dir.path(),
            "alice",
            Locale::En,
            &format!("[local]\nspeaker = \"senator::claude\"\n{secretaries}"),
        );
        desk.start().await;
        let office = desk.senate().daemon.hub.office().unwrap();
        assert_eq!(office.speaker.to_string(), "senator::claude");
        assert_eq!(office.kind, senate_core::SenatorKind::Agent);
        let expected: Vec<_> = if secretaries.is_empty() {
            vec![path("claude")]
        } else {
            vec![path("claude"), path("copilot")]
        };
        assert_eq!(office.desk, expected);
        let claude = desk.inbox("s-claude");
        let start = desk.session_start("s-claude", &claude).await.unwrap();
        assert!(
            context(&start).contains("[senator::claude] is the Speaker"),
            "{}",
            context(&start)
        );
        desk.wait_connected("claude", true).await;
        let status = desk.machine_status().await;
        let view = senate_bridge::status::collect(&desk.paths).await.unwrap();
        let text = senate_bridge::status::render(&view, chrono::Utc::now());
        assert!(
            text.contains("Speaker: [senator::claude] (an agent)"),
            "{text}"
        );
        assert!(status.roles.unwrap().present.contains(&path("claude")));
        // Its session closes: what is put to the Speaker waits, and alice
        // sees it, the person whose agent presides.
        desk.session_end("s-claude", &claude).await;
        let asked = speak(
            &desk.senate().url,
            &desk.senate().token,
            &NewEvent::new(
                committee(),
                Kind::Question,
                person("alice"),
                "Who presides while claude is away?",
            )
            .to(Address::Speaker),
        )
        .await
        .unwrap();
        persons_docket_has(&desk, asked.seq).await;
        desk.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_speaker_with_no_secretary_finds_the_desk_waiting_for_her() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::Zh, "[local]\n");
    desk.start().await;
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    // claude writes to a seat on another committee: put to the desk, where
    // no secretary works; it waits for alice.
    let asked = send(
        &desk,
        "s-claude",
        "ibot::codex",
        "Should the refund retry be capped?",
    )
    .await;
    assert_eq!(asked.posted.to, [Address::Speaker]);
    let text = persons_docket_has(&desk, asked.posted.seq).await;
    assert!(
        text.contains("Should the refund retry be capped?"),
        "{text}"
    );
    let view = senate_bridge::status::collect(&desk.paths).await.unwrap();
    let status = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(
        status.contains("Speaker：[senator::alice] (人)"),
        "{status}"
    );
    assert!(status.contains("desk：没有 secretary"), "{status}");
    desk.stop().await;
}

/// Wait until what `senate inbox` shows the person (their read of the
/// desk) lists the item `seq`; the docket as rendered.
async fn persons_docket_has(desk: &Desk, seq: senate_core::Seq) -> String {
    let docket = eventually_async("the desk in the person's inbox", || async {
        let docket: senate_bridge::ipc::DocketReply = desk
            .ipc(Request::Docket {
                session: Default::default(),
                committee: None,
            })
            .await;
        docket
            .senates
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.items)
            .any(|i| i.event.seq == seq)
            .then_some(docket)
    })
    .await;
    senate_bridge::preside::render_docket(&docket, desk.config.locale)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chair_set_for_the_committee_picks_for_senator_any() {
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, claude, copilot, _codex) = alices_machine(dir.path()).await;
    let hub = desk.senate().daemon.hub.clone();
    // Alice makes copilot the chair of refund-v2.
    desk.post("s-claude", "refund-v2 is open").await;
    let _: serde_json::Value = desk
        .ipc(Request::SetChair {
            session: Default::default(),
            committee: Some(committee()),
            senator: path("copilot"),
        })
        .await;
    assert_eq!(
        hub.chair(&committee()).await.unwrap().chair,
        Some(path("copilot"))
    );

    // Alice writes to "an agent": two are on refund-v2, so the chair picks.
    let (url, token) = (desk.senate().url.clone(), desk.senate().token.clone());
    let event = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Someone look at the flaky refund test?",
        )
        .to(any_agent()),
    )
    .await
    .unwrap();
    assert_eq!(event.to, [Address::Chair], "{event:?}");
    copilot
        .wait_for("Someone look at the flaky refund test?")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        claude
            .messages()
            .iter()
            .all(|m| !m.contains("flaky refund test")),
        "the chair picks, not the desk"
    );
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_team_principals_agent_crosses_committees_by_way_of_the_desk() {
    // A team's Senate: bob presides; `north-desk` is a team (a principal
    // that is no person), with its own token, running the agent `runner`.
    let world = Setup::new()
        .without_speaker()
        .server(|c| {
            c.roster.speaker = Some("senator::bob".parse().unwrap());
            c.roster
                .principals
                .push(Principal::of_kind(name("north-desk"), PrincipalKind::Team));
            c.tokens.push(senate_server::TokenConfig::with_token(
                name("north-desk"),
                "tok-north-desk",
            ));
        })
        .start()
        .await;
    let url = world.server_url();
    speak(
        &url,
        "tok-bob",
        &NewEvent::new(committee(), Kind::Note, person("bob"), "refund-v2 is open"),
    )
    .await
    .unwrap();
    let intended = serde_json::json!("refund-v2::senator::bob");

    // Its agent: seated under the team, and its message for another
    // committee goes to the desk on its own.
    let by_agent = speak(
        &url,
        "tok-north-desk",
        &NewEvent::new(
            ibot(),
            Kind::Question,
            senator("runner"),
            "Is refund-v2 frozen?",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, intended.clone()),
    )
    .await
    .unwrap();
    assert_eq!(by_agent.committee, ibot());
    assert_eq!(by_agent.to, [Address::Speaker]);
    let roster = world.hub().roster().await.unwrap();
    assert_eq!(roster.owner_of(&path("runner")), Some(&name("north-desk")));
    assert_eq!(
        roster.kind_of(&path("north-desk")),
        senate_core::SenatorKind::Team
    );

    // The team itself is no person either: the agent column.
    let by_team = speak(
        &url,
        "tok-north-desk",
        &NewEvent::new(
            ibot(),
            Kind::Question,
            senator("north-desk"),
            "Who reviews refunds?",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, intended.clone()),
    )
    .await
    .unwrap();
    assert_eq!(by_team.committee, ibot());
    assert_eq!(by_team.to, [Address::Speaker]);

    // A person, by contrast, is delivered there directly.
    let by_person = speak(
        &url,
        "tok-alice",
        &NewEvent::new(
            ibot(),
            Kind::Question,
            person("alice"),
            "Is refund-v2 frozen?",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, intended),
    )
    .await
    .unwrap();
    assert_eq!(by_person.committee, committee());
    assert_eq!(by_person.to, [person("bob")]);

    world.stop().await;
}

/// Both native sessions stay in their original worktrees. No secretary is
/// present to forward the discussion or its answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_committees_chair_receives_discussion_but_requests_wait_on_the_desk() {
    use senate_bridge::hook::{self, HookEnv, HookKind};
    use senate_bridge::ipc::BoundReply;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, "[local]\n");
    let sender = desk.inbox("sender");
    let chair = desk.inbox("chair");
    desk.start().await;
    for (session, committee_name, inbox) in [
        ("author", "client", &sender),
        ("maintainer", "senate", &chair),
    ] {
        let cwd = dir.path().join(committee_name);
        fs::create_dir_all(cwd.join(".senate")).unwrap();
        fs::write(cwd.join(".senate/committee"), committee_name).unwrap();
        let args =
            serde_json::json!({"session_id":session,"cwd":cwd,"source":"startup"}).to_string();
        let env = HookEnv {
            socket: Some(inbox.path.clone()),
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
    let target = CommitteeId::new("senate").unwrap();
    let source = CommitteeId::new("client").unwrap();
    let hub = desk.senate().daemon.hub.clone();
    assert_eq!(
        hub.chair(&target).await.unwrap().chair,
        Some(path("claude-maintainer"))
    );
    let message = |kind: SayKind, body: &str| Request::Send {
        session: Laptop::session("author"),
        kind,
        to: vec![],
        at: Some(senate_core::Locator::parse_short("senate::chair").unwrap()),
        body: body.into(),
        refs: None,
        id: Some(senate_core::EventId::new()),
    };
    let direct = message(SayKind::Question, "Which argument selects the destination?");
    let receipt: SendReply = desk.ipc(direct.clone()).await;
    let retry: SendReply = desk.ipc(direct).await;
    assert!(retry.posted.duplicate);
    assert_eq!(retry.posted.seq, receipt.posted.seq);
    assert_eq!(receipt.posted.committee, source);
    assert!(!receipt.on_the_desk());
    let push = chair
        .wait_for("Which argument selects the destination?")
        .await;
    assert!(push.contains("Send execution requests through the Speaker."));
    let target_thread = thread_of(&push, "Which argument selects the destination?");
    let original = hub.get(&target, target_thread).await.unwrap().unwrap();
    assert_eq!(original.on_behalf_of, Some(senator("claude-author")));
    assert_eq!(original.to, [senator("claude-maintainer")]);
    assert_eq!(
        desk.events_of(&target)
            .await
            .iter()
            .filter(|e| e.body == original.body)
            .count(),
        1
    );

    desk.answer("maintainer", target_thread, "Use the qualified address.")
        .await;
    sender.wait_for("Use the qualified address.").await;
    let back = desk
        .events_of(&source)
        .await
        .into_iter()
        .find(|e| e.body == "Use the qualified address.")
        .unwrap();
    assert_eq!(back.thread, Some(receipt.posted.seq));
    assert_eq!(back.on_behalf_of, Some(senator("claude-maintainer")));
    desk.answer("author", receipt.posted.seq, "That resolves my question.")
        .await;
    chair.wait_for("That resolves my question.").await;
    let follow_up = desk
        .events_of(&target)
        .await
        .into_iter()
        .find(|e| e.body == "That resolves my question.")
        .unwrap();
    assert_eq!(follow_up.thread, Some(target_thread));

    let note: SendReply = desk
        .ipc(message(
            SayKind::Note,
            "The client now uses the qualified address.",
        ))
        .await;
    assert!(!note.on_the_desk());
    chair
        .wait_for("The client now uses the qualified address.")
        .await;
    let request: SendReply = desk
        .ipc(message(
            SayKind::Request,
            "Please edit the client configuration.",
        ))
        .await;
    assert!(request.on_the_desk());
    assert!(
        !desk
            .events_of(&target)
            .await
            .iter()
            .any(|e| e.body == "Please edit the client configuration.")
    );
    for (id, expected) in [("author", source.clone()), ("maintainer", target.clone())] {
        let bound: BoundReply = desk
            .ipc(Request::Bound {
                session: Laptop::session(id),
            })
            .await;
        assert_eq!(bound.committee, Some(expected));
    }
    assert_eq!(
        hub.committee_agents(&target).await.unwrap(),
        [path("claude-maintainer")]
    );
    assert_eq!(
        hub.committee_agents(&source).await.unwrap(),
        [path("claude-author")]
    );
    desk.stop().await;
}

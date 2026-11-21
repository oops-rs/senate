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

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::ipc::{Forwarded, Request, SayKind, SendReply};
use senate_core::{
    Address, CommitteeId, Hub, Kind, Locale, NewEvent, Principal, PrincipalKind, meta,
};

fn ibot() -> CommitteeId {
    CommitteeId::new("ibot").unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hosted_approval_stays_on_the_docket_until_its_senator_archives_it() {
    use senate_bridge::client::ApiClient;
    use senate_core::{EventId, StatusPayload, StatusState};
    use serde_json::json;

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        "[local]\nsecretaries = [\"office-agent\"]\n",
    );
    desk.start().await;
    let chair_inbox = desk.inbox("chair");
    desk.session_start_as("live-chair", &chair_inbox, "claude")
        .await
        .unwrap();
    let office_inbox = desk.inbox("office");
    desk.session_start_as("office-session", &office_inbox, "office-agent")
        .await
        .unwrap();
    let _: senate_core::api::ChairInfo = desk
        .ipc(Request::SetChair {
            session: Default::default(),
            committee: Some(committee()),
            senator: path("claude"),
        })
        .await;
    desk.wait_connected("claude", true).await;
    let live = desk.senate();
    let api = ApiClient::new(&live.url, &live.token).unwrap();
    let hub = &live.daemon.hub;
    let waiting = senator("waiting-tui");
    let root = api
        .append(
            &NewEvent::new(
                committee(),
                Kind::Question,
                waiting.clone(),
                "Tool approval is waiting",
            )
            .to(Address::Chair)
            .with_meta(meta::HOSTED_APPROVAL, json!(true)),
        )
        .await
        .unwrap()
        .event;
    assert!(
        hub.chair_docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );
    assert!(
        !hub.docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );
    chair_inbox.wait_for("Tool approval is waiting").await;
    api.append(
        &NewEvent::new(
            committee(),
            Kind::Answer,
            senator("claude"),
            "A person can attach.",
        )
        .to(waiting.clone())
        .in_thread(root.seq),
    )
    .await
    .unwrap();
    assert!(
        hub.chair_docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );

    desk.session_end("live-chair", &chair_inbox).await;
    desk.wait_connected("claude", false).await;
    assert!(
        hub.docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );

    let archive = |sender: Address, root_id: &EventId| {
        NewEvent::new(committee(), Kind::Status, sender, "")
            .in_thread(root.seq)
            .with_meta(
                meta::STATUS,
                StatusPayload::new(StatusState::Archived).value(),
            )
            .with_meta(meta::HOSTED_APPROVAL, json!(root_id))
    };
    for invalid in [
        archive(senator("claude"), &root.id),
        archive(waiting.clone(), &EventId::new()),
    ] {
        assert_eq!(api.append(&invalid).await.unwrap_err().status(), Some(403));
    }
    api.append(&archive(waiting.clone(), &root.id))
        .await
        .unwrap();
    assert!(
        !hub.chair_docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );
    assert!(
        !hub.docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == root.id)
    );
    for (sender, final_kind) in [(waiting, Kind::Status), (person("alice"), Kind::Answer)] {
        let stranded = api
            .append(
                &NewEvent::new(committee(), Kind::Question, sender, "Stranded notice")
                    .to(Address::Speaker)
                    .with_meta(meta::HOSTED_APPROVAL, json!(true)),
            )
            .await
            .unwrap()
            .event;
        let refused: senate_bridge::ipc::Posted = desk
            .ipc(Request::Refuse {
                session: Laptop::session("office-session"),
                committee: committee(),
                seq: stranded.seq,
                reason: "This terminal no longer needs attention.".into(),
                id: None,
            })
            .await;
        let event = hub.get(&committee(), refused.seq).await.unwrap().unwrap();
        assert_eq!(event.kind, final_kind);
        assert!(
            !hub.docket(&committee())
                .await
                .unwrap()
                .items
                .iter()
                .any(|i| i.event.id == stranded.id)
        );
    }
    desk.stop().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_request_author_can_follow_up_with_the_assignee_and_other_writers() {
    use senate_bridge::ipc::{ClientError, Posted, codes};

    for locale in [Locale::En, Locale::Zh] {
        let dir = tempfile::tempdir().unwrap();
        let mut desk = Desk::new(dir.path(), "alice", locale, "[local]\n");
        desk.start().await;
        let author = desk.inbox("author");
        let assignee = desk.inbox("assignee");
        let contributor = desk.inbox("contributor");
        for (session, inbox, agent) in [
            ("author", &author, "claude"),
            ("assignee", &assignee, "codex"),
            ("contributor", &contributor, "copilot"),
        ] {
            desk.session_start_as(session, inbox, agent).await.unwrap();
        }
        let request: Posted = desk
            .ipc(Request::Ask {
                session: Laptop::session("author"),
                to: vec![senator("codex")],
                body: "Update the retry tests.".into(),
                refs: None,
                id: None,
                request: true,
            })
            .await;
        assignee.wait_for("Update the retry tests.").await;

        let answer = desk
            .answer("assignee", request.seq, "I will update the retry tests.")
            .await;
        assert_eq!(answer.to, [senator("claude")]);
        author.wait_for("I will update the retry tests.").await;
        let third = desk
            .answer(
                "contributor",
                request.seq,
                "The fixture already tests the first retry.",
            )
            .await;
        assert_eq!(third.to, [senator("claude")]);
        author
            .wait_for("The fixture already tests the first retry.")
            .await;

        let hub = &desk.senate().daemon.hub;
        hub.append(
            NewEvent::new(
                committee(),
                Kind::Note,
                senator("codex"),
                "Note for a copied recipient.",
            )
            .to(senator("observer"))
            .in_thread(request.seq),
        )
        .await
        .unwrap();
        let followup = desk
            .answer("author", answer.seq, "Please cover the timeout case too.")
            .await;
        assert_eq!(followup.thread, Some(request.seq));
        assert_eq!(followup.to, [senator("codex"), senator("copilot")]);
        assignee
            .wait_for("Please cover the timeout case too.")
            .await;
        contributor
            .wait_for("Please cover the timeout case too.")
            .await;

        let relayed = hub
            .append(
                NewEvent::new(
                    committee(),
                    Kind::Request,
                    Address::Speaker,
                    "Please review the timeout fixture.",
                )
                .on_behalf_of(senator("claude"))
                .to(senator("codex")),
            )
            .await
            .unwrap()
            .event;
        let followup = desk
            .answer("author", relayed.seq, "The fixture is ready for review.")
            .await;
        assert_eq!(followup.to, [senator("codex")]);
        assignee.wait_for("The fixture is ready for review.").await;

        let solo = desk
            .post("author", "A note with no other participant.")
            .await;
        let error = desk
            .try_ipc::<Posted>(Request::Answer {
                session: Laptop::session("author"),
                thread: solo.seq,
                body: "There is no recipient for this follow-up.".into(),
                id: None,
                committee: None,
                agent: None,
                as_person: false,
            })
            .await
            .unwrap_err();
        let reason = locale.pick(
            "没有其他参与者可以接收回复",
            "There is nobody else to reply to",
        );
        assert!(
            matches!(&error, ClientError::Remote(e)
            if e.code == codes::BAD_REQUEST && e.message.contains(reason)),
            "{error:?}"
        );
        let events = desk.events_of(&committee()).await;
        assert!(
            !events.iter().any(|e| e.thread == Some(solo.seq)),
            "{events:?}"
        );
        desk.stop().await;
    }
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
async fn chair_handover_notifies_both_sessions_and_changes_their_tools() {
    use senate_bridge::mcp::{CHAIR_TOOLS, SenateMcp};
    use senate_core::{ChairPayload, ChairSource, Seq};

    async fn has_chair_tools(mcp: &SenateMcp, expected: bool) {
        let tools = mcp.tools().await;
        for name in CHAIR_TOOLS {
            assert_eq!(tools.iter().any(|tool| tool.name == name), expected);
        }
    }

    for locale in [Locale::En, Locale::Zh] {
        let dir = tempfile::tempdir().unwrap();
        let mut desk = Desk::new(dir.path(), "alice", locale, "[local]\n");
        desk.start().await;
        let outgoing = desk.inbox("outgoing");
        let incoming = desk.inbox("incoming");
        let outgoing_ctx = desk
            .session_start_as("outgoing", &outgoing, "claude")
            .await
            .unwrap();
        let incoming_ctx = desk
            .session_start_as("incoming", &incoming, "codex")
            .await
            .unwrap();
        let chair_text = locale.pick(
            "你是本委员会的主席（chair），负责主持委员会。",
            "You are the chair of this committee and are in charge of it.",
        );
        let member_text = |chair: &str| {
            if locale == Locale::Zh {
                format!("你是委员会成员。委员会的主席（chair）是 [senator::{chair}]。")
            } else {
                format!("You are a committee member. The chair is [senator::{chair}].")
            }
        };
        assert!(
            context(&outgoing_ctx).contains(chair_text),
            "{outgoing_ctx}"
        );
        for name in CHAIR_TOOLS {
            assert!(context(&outgoing_ctx).contains(&format!("mcp__senate__{name}")));
        }
        let member = context(&incoming_ctx);
        assert!(member.contains(&member_text("claude")), "{member}");
        for tool in ["ask", "status", "answer", "escalate"] {
            assert!(member.contains(&format!("mcp__senate__{tool}")), "{member}");
        }
        assert!(member.contains(locale.pick("省略 to", "without to")));

        let outgoing_mcp = SenateMcp::new(desk.paths.socket(), Laptop::session("outgoing"));
        let incoming_mcp = SenateMcp::new(desk.paths.socket(), Laptop::session("incoming"));
        has_chair_tools(&outgoing_mcp, true).await;
        has_chair_tools(&incoming_mcp, false).await;

        let info: senate_core::api::ChairInfo = desk
            .ipc(Request::SetChair {
                session: Default::default(),
                committee: Some(committee()),
                senator: path("codex"),
            })
            .await;
        assert_eq!(info.chair, Some(path("codex")));
        assert_eq!(info.source, Some(ChairSource::Set));
        let notice = locale.pick(
            "[senator::codex] 现任委员会 `refund-v2` 的主席（chair）。[senator::alice] 免去了 [senator::claude] 的主席职务，[senator::claude] 仍是委员会成员。",
            "[senator::codex] now chairs the committee `refund-v2`. [senator::claude] has been replaced as chair by [senator::alice] and remains a committee member.",
        );
        outgoing.wait_for(notice).await;
        incoming.wait_for(notice).await;

        let hub = &desk.senate().daemon.hub;
        assert_eq!(hub.chair(&committee()).await.unwrap(), info);
        let log = hub.read(&committee(), Seq::ZERO, 100).await.unwrap();
        let change = log
            .iter()
            .find(|event| ChairPayload::of(event).is_some())
            .unwrap();
        assert_eq!(change.to, [senator("codex"), senator("claude")]);
        assert_eq!(change.body, notice);
        let status = senate_bridge::status::collect(&desk.paths).await.unwrap();
        assert!(
            status
                .roles
                .unwrap()
                .chairs
                .iter()
                .any(|chair| { chair.committee == info.committee && chair.chair == info.chair })
        );
        has_chair_tools(&outgoing_mcp, false).await;
        has_chair_tools(&incoming_mcp, true).await;
        let old_role_call = desk
            .try_ipc::<serde_json::Value>(Request::ChairDocket {
                session: Laptop::session("outgoing"),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(old_role_call, senate_bridge::ipc::ClientError::Remote(e)
            if e.code == senate_bridge::ipc::codes::NOT_CHAIRING)
        );
        let _: serde_json::Value = desk
            .ipc(Request::ChairDocket {
                session: Laptop::session("incoming"),
            })
            .await;

        let outgoing_ctx = desk
            .session_start_as("outgoing", &outgoing, "claude")
            .await
            .unwrap();
        let incoming_ctx = desk
            .session_start_as("incoming", &incoming, "codex")
            .await
            .unwrap();
        assert!(context(&outgoing_ctx).contains(&member_text("codex")));
        assert!(context(&incoming_ctx).contains(chair_text));

        let _: senate_core::api::CommitteeSeatInfo = desk
            .ipc(Request::SetCommitteeSeat {
                committee: committee(),
                request: senate_core::api::SetCommitteeSeat {
                    senator: path("codex"),
                    action: senate_core::CommitteeSeatAction::Unseat,
                    id: senate_core::EventId::new(),
                    acted_by: None,
                },
            })
            .await;
        let succession = locale.pick(
            "[senator::codex] 被除席后，[senator::claude] 接任委员会 `refund-v2` 的主席（chair）。",
            "[senator::claude] now chairs the committee `refund-v2` after [senator::codex] was unseated.",
        );
        outgoing.wait_for(succession).await;
        let fallback = hub.chair(&committee()).await.unwrap();
        assert_eq!(fallback.chair, Some(path("claude")));
        assert_eq!(fallback.source, Some(ChairSource::FirstAgent));
        has_chair_tools(&outgoing_mcp, true).await;
        has_chair_tools(&incoming_mcp, false).await;
        desk.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawning_checks_scope_and_capacity_on_the_daemon() {
    use senate_bridge::ipc::{ClientError, HostedRuntime, SpawnReply, codes};
    use senate_bridge::mcp::SenateMcp;

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        "hosted_session_limit = 0\n[local]\nsecretaries = [\"office\"]\n",
    );
    desk.start().await;
    let chair = desk.inbox("chair");
    let member = desk.inbox("member");
    let office = desk.inbox("office");
    desk.session_start_as("chair", &chair, "claude")
        .await
        .unwrap();
    desk.session_start_as("member", &member, "codex")
        .await
        .unwrap();
    desk.session_start_as("office", &office, "office")
        .await
        .unwrap();
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

    for (session, allowed) in [("chair", true), ("office", true), ("member", false)] {
        let mcp = SenateMcp::new(desk.paths.socket(), Laptop::session(session));
        assert_eq!(
            mcp.tools().await.iter().any(|tool| tool.name == "spawn"),
            allowed
        );
    }
    let spawn = |session, committee, chair| Request::Spawn {
        session,
        committee,
        runtime: HostedRuntime::Codex,
        alias: None,
        cwd: dir.path().to_path_buf(),
        chair,
        prompt: None,
        model: Default::default(),
    };
    for (session, committee, chair, code, message) in [
        (
            "member",
            committee(),
            false,
            codes::NOT_CHAIRING,
            "only the Speaker's office or this committee's chair",
        ),
        (
            "chair",
            ibot(),
            false,
            codes::NOT_PRESIDING,
            "another committee",
        ),
        (
            "chair",
            committee(),
            true,
            codes::NOT_PRESIDING,
            "office may start a chair",
        ),
        (
            "chair",
            committee(),
            false,
            codes::BAD_REQUEST,
            "hosted session limit (0)",
        ),
        (
            "office",
            ibot(),
            true,
            codes::BAD_REQUEST,
            "hosted session limit (0)",
        ),
    ] {
        let error = desk
            .try_ipc::<SpawnReply>(spawn(Laptop::session(session), committee, chair))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::Remote(e) if e.code == code && e.message.contains(message)),
            "{error:?}"
        );
    }
    let cli = desk
        .try_ipc::<SpawnReply>(spawn(Default::default(), committee(), false))
        .await
        .unwrap_err();
    assert!(
        matches!(&cli, ClientError::Remote(e) if e.message.contains("hosted session limit (0)"))
    );
    assert!(!desk.paths.home().join("hosts").exists());
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restoring_an_unseated_senator_requires_explicit_rejoin_before_capacity() {
    use senate_bridge::ipc::{ClientError, JoinReply, SpawnReply, codes};
    use senate_core::CommitteeSeatAction;
    use senate_core::api::{CommitteeSeatInfo, SetCommitteeSeat};

    const SESSION: &str = "00000000-0000-4000-8000-000000000123";
    let primary = path(&format!("codex-{SESSION}"));
    let alias = path("restore-target");
    let committee = committee();
    let join = format!("senate seat join --committee {committee} --senator {primary}");
    for locale in [Locale::En, Locale::Zh] {
        let dir = tempfile::tempdir().unwrap();
        // The cap would refuse a seated target. The seat refusal must
        // happen before the daemon reaches the launch path.
        let mut desk = Desk::new(
            dir.path(),
            "alice",
            locale,
            "hosted_session_limit = 0\n[local]\n",
        );
        desk.start().await;
        let inbox = desk.inbox("chair");
        desk.session_start_as("chair", &inbox, "claude")
            .await
            .unwrap();
        desk.codex_start_with_identity(SESSION, true).await.unwrap();
        let _: JoinReply = desk
            .ipc(Request::JoinAs {
                session: Laptop::session(SESSION),
                committee: committee.clone(),
                senator: alias.clone(),
            })
            .await;
        desk.codex_end(SESSION).await;

        let seat = |action| Request::SetCommitteeSeat {
            committee: committee.clone(),
            request: SetCommitteeSeat {
                senator: primary.clone(),
                action,
                id: senate_core::EventId::new(),
                acted_by: None,
            },
        };
        let _: CommitteeSeatInfo = desk.ipc(seat(CommitteeSeatAction::Unseat)).await;
        let expected = match locale {
            Locale::En => format!(
                "[senator::{primary}] was unseated from committee `{committee}`. Run `{join}` before restoring."
            ),
            Locale::Zh => format!(
                "[senator::{primary}] 已被委员会 `{committee}` 除席。请先运行 `{join}`，再恢复会话。"
            ),
        };
        for session in [Default::default(), Laptop::session("chair")] {
            let error = desk
                .try_ipc::<SpawnReply>(Request::Restore {
                    session,
                    senator: alias.clone().into(),
                    model: Default::default(),
                })
                .await
                .unwrap_err();
            assert!(
                matches!(&error, ClientError::Remote(e)
                if e.code == codes::BAD_REQUEST && e.message == expected),
                "{error:?}"
            );
        }
        assert!(!desk.paths.home().join("hosts").exists());

        let _: CommitteeSeatInfo = desk.ipc(seat(CommitteeSeatAction::Join)).await;
        let error = desk
            .try_ipc::<SpawnReply>(Request::Restore {
                session: Laptop::session("chair"),
                senator: alias.clone().into(),
                model: Default::default(),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ClientError::Remote(e)
            if e.message.contains("hosted session limit (0)")),
            "{error:?}"
        );
        desk.stop().await;
    }
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

    // The source root has a server-pinned peer even before anyone writes
    // back from the target committee.
    let early = desk
        .answer(
            "author",
            receipt.posted.seq,
            "This is about address resolution.",
        )
        .await;
    assert!(early.to.is_empty());
    chair.wait_for("This is about address resolution.").await;
    let early_copy = desk
        .events_of(&target)
        .await
        .into_iter()
        .find(|event| event.body == "This is about address resolution.")
        .unwrap();
    assert_eq!(early_copy.thread, Some(target_thread));
    assert_eq!(early_copy.to, [senator("claude-maintainer")]);

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

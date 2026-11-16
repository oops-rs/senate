//! Senate alone on one machine: `senated --machine` hosting a local top
//! Senate whose Speaker is one of the person's agents, `alice/claude` (a
//! Claude Code session), with `alice/codex` (a Codex session) seated beside
//! it. No Senate above, no built-in Speaker, no chat platform.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::fake_codex::input_text;
use common::*;
use senate_bridge::ipc::{ClientError, DocketReply, Posted, PresidesReply, Request};
use senate_bridge::session::Runtime;
use senate_core::{
    Address, Awaiting, Hub, Kind, Locale, NewEvent, PickPayload, Seq, SpeakerSeat, is_takeover,
};

const ALONE: &str = "[local]\nspeaker = \"claude\"\n";

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
    desk.git_init("feature/refund-v2/alice");
    let codex = desk.codex_app_server();
    desk.start().await;
    let top = desk.senate(None);
    let (url, token) = (top.url.clone(), top.token.clone());

    // The local top Senate: alice alone, alice/claude in the Speaker's seat.
    let me = me_at(&url, &token).await;
    assert_eq!(me.person.as_str(), "alice");
    assert_eq!(me.presider, None);
    assert_eq!(me.speaker, Some(SpeakerSeat::Senator(path("alice/claude"))));
    assert!(!me.triaged);

    // The Claude Code session presides, and is told so.
    let claude = desk.inbox("s-claude");
    let start = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&start).contains("of the top Senate as [senator::alice/claude]"),
        "{}",
        context(&start)
    );
    assert!(
        context(&start).contains("[senator::alice/claude] is the Speaker of the top Senate"),
        "{}",
        context(&start)
    );
    // The Codex session sits beside it, and does not preside.
    let start = desk.codex_start("thread-codex").await.unwrap();
    assert!(context(&start).contains("[senator::alice/codex]"));
    assert!(!context(&start).contains("is the Speaker"));
    codex.idle("thread-codex");
    desk.wait_connected(None, "alice/claude", true).await;
    desk.wait_connected(None, "alice/codex", true).await;
    let presides: PresidesReply = desk
        .ipc(Request::Presides {
            session: Laptop::session("thread-codex"),
            runtime: Runtime::Codex,
            agent_env: None,
            agent_file: None,
        })
        .await;
    assert_eq!(presides.agent, Some(path("alice/codex")));
    assert!(presides.presiding.is_none());

    // Codex asks Claude and gets an answer, at once: no verdict is awaited.
    let asked = desk
        .ask(
            "thread-codex",
            senator("alice/claude"),
            "Which header carries the idempotency key?",
        )
        .await;
    let push = claude.wait_for("idempotency key").await;
    assert!(push.contains("from [senator::alice/codex]"), "{push}");
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

    // Alice, as herself, writes to "her agent": she has two here, so it is
    // put to the presiding session to route.
    let picked = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Please check the refund retry limit.",
        )
        .to(agent_of("alice")),
    )
    .await
    .unwrap();
    assert_eq!(picked.to, [Address::Speaker]);
    let mut candidates = PickPayload::of(&picked).unwrap().candidates;
    candidates.sort();
    assert_eq!(candidates, [path("alice/claude"), path("alice/codex")]);
    let put = claude.wait_for("refund retry limit").await;
    assert!(
        put.contains("for you as Speaker of the top Senate"),
        "{put}"
    );
    assert!(put.contains("speaker_route"), "{put}");
    let docket: DocketReply = desk
        .ipc(Request::Docket {
            session: Laptop::session("s-claude"),
            committee: None,
        })
        .await;
    assert_eq!(docket.speaker, Some(path("alice/claude")));
    let page = &docket.senates[0].pages[0];
    let item = page
        .items
        .iter()
        .find(|i| i.event.seq == picked.seq)
        .unwrap();
    assert!(matches!(&item.awaiting, Awaiting::Pick { candidates, .. } if candidates.len() == 2));
    // The facts to pick by.
    let codex_seat = page
        .seats
        .iter()
        .find(|s| s.agent == path("alice/codex"))
        .unwrap();
    assert!(codex_seat.connected);
    // Codex may not route it: it does not preside.
    let refused = desk
        .try_ipc::<Posted>(Request::Route {
            session: Laptop::session("thread-codex"),
            committee: committee(),
            seq: picked.seq,
            to: path("alice/codex"),
            id: None,
        })
        .await
        .unwrap_err();
    let ClientError::Remote(refused) = refused else {
        panic!("{refused}")
    };
    assert_eq!(refused.code, "not_presiding");
    // The presiding session routes it to Codex, in Alice's name.
    let routed: Posted = desk
        .ipc(Request::Route {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: picked.seq,
            to: path("alice/codex"),
            id: None,
        })
        .await;
    let relayed = eventually("codex is relayed it", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("refund retry limit"))
    })
    .await;
    assert!(relayed.contains("[speaker::alice]"), "{relayed}");
    let relay = top
        .daemon
        .hub
        .get(&committee(), routed.seq)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(relay.from, Address::Speaker);
    assert_eq!(relay.to, [senator("alice/codex")]);

    // Codex leaves a checkpoint, then goes away.
    desk.checkpoint(
        "thread-codex",
        Some("retry limit: 3 attempts, see refund/retry.rs"),
        None,
        None,
    )
    .await;
    desk.codex_end("thread-codex").await;
    desk.wait_connected(None, "alice/codex", false).await;

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
        .to(senator("alice/codex")),
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
            agents: vec![path("alice/codex")]
        }
    );
    let checkpoint = &senate.checkpoints[0];
    assert_eq!(checkpoint.agent, path("alice/codex"));
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
            to: path("alice/claude"),
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
    assert_eq!(answer.on_behalf_of, Some(senator("alice/codex")));
    assert_eq!(answer.to, [person("alice")]);
    assert!(is_takeover(&answer));
    assert_eq!(
        answer.byline(Locale::En).unwrap(),
        "[speaker::alice/codex] takeover"
    );
    assert_eq!(
        answer.byline(Locale::Zh).unwrap(),
        "[speaker::alice/codex] 接管"
    );
    assert_eq!(
        answer.refs.as_ref().unwrap().branch.as_deref(),
        Some("feature/refund-v2/alice")
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
    desk.wait_connected(None, "alice/claude", false).await;
    let own = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Where does the idempotency key live?",
        )
        .to(senator("alice/claude")),
    )
    .await
    .unwrap();
    let page = top.daemon.hub.docket(&committee()).await.unwrap();
    assert!(
        page.items.iter().all(|i| i.event.seq != own.seq),
        "{:?}",
        page.items
    );
    assert!(
        page.items
            .iter()
            .all(|i| !matches!(&i.awaiting, Awaiting::Away { agents } if agents.contains(&path("alice/claude")))),
    );
    let refused = desk
        .try_ipc::<Posted>(Request::Cover {
            session: Laptop::session("s-claude"),
            committee: committee(),
            thread: own.seq,
            body: "from nowhere".into(),
            agent: Some(path("alice/claude")),
            id: None,
        })
        .await;
    assert!(refused.is_err(), "no covering for the seat holder");
    desk.stop().await;
}

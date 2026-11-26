//! An agent as another agent's secretary: while `codex` is away, a question
//! to it is brought to the session of `claude`, its secretary, with
//! codex's own latest checkpoint and notes, and claude answers in codex's
//! name; a request is never performed; when codex returns it is told what
//! was answered for it; and with claude away too the question reaches the
//! chair and the Speaker's desk.
//!
//! One machine, one person (alice, the Speaker). `claude` is codex's
//! secretary, `copilot` chairs the committee, `reviewer` works the desk.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::Duration;

use common::fake_codex::input_text;
use common::*;
use senate_bridge::ipc::{
    ChairDocketReply, ClientError, DocketReply, Posted, Request, SecretaryOp,
};
use senate_core::{Awaiting, Hub, Kind, Locale, NewEvent, meta};

const CONFIG: &str = "[local]\nsecretaries = [\"reviewer\"]\n";

fn codex_pushes(codex: &common::fake_codex::FakeCodex) -> Vec<String> {
    codex
        .pushes()
        .iter()
        .map(|(_, params)| input_text(params).to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_secretary_covers_an_away_agents_questions_from_its_notes() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, CONFIG);
    desk.git_init("senate/refund-v2/alice");
    let codex = desk.codex_app_server();
    desk.start().await;
    let top = desk.senate();
    let (url, token) = (top.url.clone(), top.token.clone());

    // The sessions: claude, copilot and reviewer (Claude Code), codex.
    let claude = desk.inbox("s-claude");
    desk.session_start_as("s-claude", &claude, "claude")
        .await
        .unwrap();
    let copilot = desk.inbox("s-copilot");
    desk.session_start_as("s-copilot", &copilot, "copilot")
        .await
        .unwrap();
    let reviewer = desk.inbox("s-reviewer");
    desk.session_start_as("s-reviewer", &reviewer, "reviewer")
        .await
        .unwrap();
    desk.codex_start("thread-codex").await.unwrap();
    codex.idle("thread-codex");
    for agent in ["claude", "copilot", "reviewer", "codex"] {
        desk.wait_connected(agent, true).await;
    }

    // claude is codex's secretary (codex registers it); copilot chairs.
    let registered: senate_bridge::ipc::SecretariesReply = desk
        .ipc(Request::Secretaries {
            session: Laptop::session("thread-codex"),
            action: SecretaryOp::Add,
            for_: None,
            secretary: Some(path("claude")),
        })
        .await;
    assert!(registered.secretaries.contains(&path("claude")));
    let _: serde_json::Value = desk
        .ipc(Request::SetChair {
            session: Laptop::session("s-reviewer"),
            committee: Some(committee()),
            senator: path("copilot"),
        })
        .await;

    // codex leaves a checkpoint and goes away.
    desk.checkpoint(
        "thread-codex",
        Some("retry limit: 3 attempts, see refund/retry.rs"),
        Some("backoff is exponential"),
        None,
    )
    .await;
    desk.codex_end("thread-codex").await;
    desk.wait_connected("codex", false).await;

    // A question to codex: claude is pushed it, with codex's own notes.
    let asked = speak(
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
    let push = claude.wait_for("What is the retry limit?").await;
    assert!(
        push.contains("whose secretary you are (it is away)"),
        "{push}"
    );
    assert!(push.contains("retry limit: 3 attempts"), "{push}");
    assert!(push.contains("backoff is exponential"), "{push}");
    assert!(
        push.contains(&format!(
            "mcp__senate__secretary_cover(thread={})",
            asked.seq
        )),
        "{push}"
    );
    // It is the secretary's now: not on the chair's docket nor the desk's.
    let chair: ChairDocketReply = desk
        .ipc(Request::ChairDocket {
            session: Laptop::session("s-copilot"),
        })
        .await;
    assert!(
        chair.page.items.iter().all(|i| i.event.seq != asked.seq),
        "{:?}",
        chair.page.items
    );
    let on_the_desk: DocketReply = desk
        .ipc(Request::Docket {
            session: Laptop::session("s-reviewer"),
            committee: Some(committee()),
        })
        .await;
    assert!(
        on_the_desk.senates[0].pages[0]
            .items
            .iter()
            .all(|i| i.event.seq != asked.seq)
    );

    // A request to codex is not claude's to perform: it is not pushed, and
    // covering it is refused; it waits for codex.
    let request = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Request,
            person("alice"),
            "Please rename the retry flag.",
        )
        .to(senator("codex")),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        claude
            .messages()
            .iter()
            .all(|m| !m.contains("rename the retry flag")),
        "{:?}",
        claude.messages()
    );
    let refused = desk
        .try_ipc::<Posted>(Request::SecretaryCover {
            session: Laptop::session("s-claude"),
            thread: request.seq,
            body: "renamed".into(),
            agent: None,
            id: None,
        })
        .await;
    assert!(
        matches!(&refused, Err(ClientError::Remote(e)) if e.message.contains("never performs a request")),
        "{refused:?}"
    );

    // claude answers in codex's name.
    let answered: Posted = desk
        .ipc(Request::SecretaryCover {
            session: Laptop::session("s-claude"),
            thread: asked.seq,
            body: "3 attempts (per its notes).".into(),
            agent: None,
            id: None,
        })
        .await;
    let answer = top
        .daemon
        .hub
        .get(&committee(), answered.seq)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.from, senator("claude"));
    assert_eq!(answer.on_behalf_of, Some(senator("codex")));
    assert_eq!(answer.to, [person("alice")]);
    assert_eq!(answer.thread, Some(asked.seq));
    assert_eq!(
        answer.meta.get(meta::TAKEOVER),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        answer.refs.as_ref().unwrap().branch.as_deref(),
        Some("senate/refund-v2/alice")
    );
    assert_eq!(
        senate_bridge::format::author(&answer, Locale::En),
        "[senator::codex] (answered by its secretary [senator::claude])"
    );
    assert_eq!(
        senate_bridge::format::author(&answer, Locale::Zh),
        "[senator::codex]（由其 secretary [senator::claude] 代答）"
    );
    // A second answer for it is refused: it is answered.
    let again = desk
        .try_ipc::<Posted>(Request::SecretaryCover {
            session: Laptop::session("s-claude"),
            thread: asked.seq,
            body: "again".into(),
            agent: None,
            id: None,
        })
        .await;
    assert!(again.is_err(), "answered already");

    // codex returns: it is told what was answered in its name, and the
    // request that waited for it is delivered to it.
    desk.codex_start("thread-codex").await.unwrap();
    codex.idle("thread-codex");
    desk.wait_connected("codex", true).await;
    let told = eventually("codex told what was answered for it", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("3 attempts (per its notes)."))
    })
    .await;
    assert!(
        told.contains("answered by its secretary [senator::claude]"),
        "{told}"
    );
    assert!(
        told.contains("while you were away your secretary [senator::claude] answered this"),
        "{told}"
    );
    let waited = eventually("codex gets the request", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("rename the retry flag"))
    })
    .await;
    assert!(!waited.contains("secretary_cover"), "{waited}");
    // The question itself, answered in its name, is not delivered to it.
    assert!(
        codex_pushes(&codex)
            .iter()
            .all(|p| !p.contains("What is the retry limit?")),
        "{:?}",
        codex_pushes(&codex)
    );

    // codex away again and claude away too: no secretary is present, so the
    // chain goes on to the chair, then to the desk.
    desk.checkpoint("thread-codex", Some("retry limit: 3 attempts"), None, None)
        .await;
    desk.codex_end("thread-codex").await;
    desk.wait_connected("codex", false).await;
    desk.session_end("s-claude", &claude).await;
    desk.wait_connected("claude", false).await;
    let next = speak(
        &url,
        &token,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Which file holds the limit?",
        )
        .to(senator("codex")),
    )
    .await
    .unwrap();
    let chair: ChairDocketReply = desk
        .ipc(Request::ChairDocket {
            session: Laptop::session("s-copilot"),
        })
        .await;
    let item = chair
        .page
        .items
        .iter()
        .find(|i| i.event.seq == next.seq)
        .expect("on the chair's docket");
    assert_eq!(
        item.awaiting,
        Awaiting::Away {
            agents: vec![path("codex")]
        }
    );
    assert!(
        chair.checkpoints.iter().any(|c| c.agent == path("codex")),
        "the chair sees codex's checkpoint"
    );
    let on_the_desk: DocketReply = desk
        .ipc(Request::Docket {
            session: Laptop::session("s-reviewer"),
            committee: Some(committee()),
        })
        .await;
    assert!(
        on_the_desk.senates[0].pages[0]
            .items
            .iter()
            .any(|i| i.event.seq == next.seq),
        "on the desk's docket"
    );
    // And claude, away, is not pushed it.
    assert!(
        claude
            .messages()
            .iter()
            .all(|m| !m.contains("Which file holds the limit?")),
        "{:?}",
        claude.messages()
    );
    desk.stop().await;
}

/// The agent returns before the secretary has been pushed the question: the
/// secretary's item is withdrawn (it is never pushed to it afterwards), the
/// agent itself gets the question, and the secretary may not answer it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_secretarys_item_is_withdrawn_when_the_agent_returns_first() {
    use senate_bridge::state::{ItemStatus, Store};

    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, CONFIG);
    desk.git_init("senate/refund-v2/alice");
    let codex = desk.codex_app_server();
    desk.start().await;
    let top = desk.senate();
    let (url, token) = (top.url.clone(), top.token.clone());

    // claude is the one that goes away; codex is its secretary, whose
    // Codex thread is not ready to take a push (it was never idle), so what
    // is queued for it waits.
    let claude = desk.inbox("s-claude");
    desk.session_start_as("s-claude", &claude, "claude")
        .await
        .unwrap();
    desk.codex_start("thread-codex").await.unwrap();
    desk.wait_connected("claude", true).await;
    desk.wait_connected("codex", true).await;
    let _: senate_bridge::ipc::SecretariesReply = desk
        .ipc(Request::Secretaries {
            session: Laptop::session("s-claude"),
            action: SecretaryOp::Add,
            for_: None,
            secretary: Some(path("codex")),
        })
        .await;
    desk.checkpoint(
        "s-claude",
        Some("the key is in gateway/idem.rs"),
        None,
        None,
    )
    .await;
    desk.session_end("s-claude", &claude).await;
    desk.wait_connected("claude", false).await;

    let asked = speak(
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
    // The server says codex covers it.
    eventually_async("the server names codex as the cover", || async {
        let page = top.daemon.hub.docket(&committee()).await.unwrap();
        (page.covered.get(&asked.seq) == Some(&path("codex"))).then_some(())
    })
    .await;
    // See the actual local cover enqueue and the adapter's attempt to
    // deliver it to the still-unloaded thread.
    let store = Store::open_read_only(&desk.paths.state_db()).unwrap();
    let local = |agent: &str| store.item(&path(agent), &committee(), asked.seq).unwrap();
    eventually("the local secretary cover was queued and tried", || {
        let waiting = local("codex")
            .is_some_and(|item| matches!(item.status, ItemStatus::Pending | ItemStatus::Queued));
        let tried = codex.calls().iter().any(|(method, params)| {
            method == "thread/read" && params["threadId"] == "thread-codex"
        });
        (waiting && tried).then_some(())
    })
    .await;
    assert!(codex_pushes(&codex).is_empty(), "the thread is not ready");

    // claude comes back before anything was pushed to codex.
    let again = desk.inbox("s-claude-2");
    let hook = desk
        .session_start_as("s-claude-2", &again, "claude")
        .await
        .unwrap();
    desk.wait_connected("claude", true).await;
    let question = "Where does the idempotency key live?";
    let owns = if context(&hook).contains(question) {
        context(&hook).to_owned()
    } else {
        again.wait_for(question).await
    };
    assert!(
        !owns.contains("↳ as [senator::claude]'s secretary"),
        "{owns}"
    );
    eventually(
        "the question was delivered to the returning native session",
        || {
            let item = local("claude")?;
            (item.status == ItemStatus::Delivered
                && item.session_id.as_deref() == Some("s-claude-2"))
            .then_some(())
        },
    )
    .await;
    eventually_async("the question is no longer the cover's", || async {
        let page = top.daemon.hub.docket(&committee()).await.unwrap();
        page.covered.is_empty().then_some(())
    })
    .await;
    eventually("the daemon withdrew the queued cover", || {
        (local("codex")?.status == ItemStatus::Cancelled).then_some(())
    })
    .await;
    codex.idle("thread-codex");
    // Its next hook is what would flush anything held for it.
    desk.codex_start("thread-codex").await.unwrap();
    let marker = "Codex can take ordinary mail again.";
    speak(
        &url,
        &token,
        &NewEvent::new(committee(), Kind::Note, person("alice"), marker).to(senator("codex")),
    )
    .await
    .unwrap();
    eventually("Codex received newer mail after the withdrawal", || {
        codex_pushes(&codex)
            .iter()
            .any(|text| text.contains(marker))
            .then_some(())
    })
    .await;
    assert!(
        codex_pushes(&codex)
            .iter()
            .all(|p| !p.contains("idempotency key")),
        "{:?}",
        codex_pushes(&codex)
    );
    let refused = desk
        .try_ipc::<Posted>(Request::SecretaryCover {
            session: Laptop::session("thread-codex"),
            thread: asked.seq,
            body: "gateway/idem.rs".into(),
            agent: None,
            id: None,
        })
        .await;
    assert!(
        matches!(&refused, Err(ClientError::Remote(e)) if e.message.contains("not yours to answer")),
        "{refused:?}"
    );
    desk.stop().await;
}

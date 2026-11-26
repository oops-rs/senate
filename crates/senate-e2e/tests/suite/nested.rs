//! No Senate nests under an agent any more: a machine hosts one Senate, and
//! a session's agent is one flat name. A session pinned to a path
//! (`SENATE_AGENT=claude/reviewer`, the old way to sit one level down) is
//! not seated under it: the pin is ignored and the harness's name is used.
//! An old home that still lists `[[preside]]` starts its one Senate and
//! leaves the nested one's log alone, unhosted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::*;
use senate_bridge::agent::AgentSource;
use senate_bridge::ipc::{Request, StatusReply};
use senate_core::{Hub, Locale, SpeakerSeat};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_pinned_to_a_path_is_seated_under_its_harness_name() {
    let world = Setup::new().without_chat().start().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let reviewer = alice.inbox("s-reviewer");
    let start = alice
        .session_start_as("s-reviewer", &reviewer, "claude/reviewer")
        .await
        .expect("orientation");
    assert!(
        context(&start).contains("[senator::claude]"),
        "seated under the harness's name: {}",
        context(&start)
    );
    assert!(
        !context(&start).contains("reviewer"),
        "no path, no nested seat: {}",
        context(&start)
    );
    world.wait_connected("claude", true).await;

    let status: StatusReply = alice.ipc(Request::Status).await;
    let session = status
        .sessions
        .iter()
        .find(|s| s.session_id == "s-reviewer")
        .unwrap();
    assert_eq!(session.agent, path("claude"));
    assert_eq!(session.agent_source, AgentSource::Harness);

    // The Senate seats `claude`, run by alice, and nothing under it.
    let roster = world.hub().roster().await.unwrap();
    assert_eq!(roster.owner_of(&path("claude")), Some(&name("alice")));
    assert!(
        roster.agents().iter().all(|a| a.as_str() != "reviewer"),
        "{:?}",
        roster.agents()
    );
    alice.close().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_home_with_preside_starts_its_one_senate_without_the_nested_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        "[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n",
    );
    // What the nested Senate left behind: its log.
    let under = desk.paths.hosted("under-claude");
    std::fs::create_dir_all(&under).unwrap();
    std::fs::write(under.join("log.db"), b"an old log").unwrap();
    desk.start().await;

    // One Senate, the machine's. Its `speaker = "claude"` is read as the
    // person presiding with claude as her secretary, first at the desk.
    let keys: Vec<&str> = desk
        .machine()
        .senates()
        .iter()
        .map(|s| s.key.as_str())
        .collect();
    assert_eq!(keys, ["top"]);
    let top = desk.senate();
    assert_eq!(top.name.as_str(), "alice-desk");
    let me = me_at(&top.url, &top.token).await;
    assert_eq!(me.speaker, Some(SpeakerSeat::Senator(path("claude"))));
    assert_eq!(me.parent, None);
    let office = me.office.expect("an office");
    assert_eq!(office.speaker.to_string(), "senator::alice");
    assert_eq!(office.desk, [path("claude")]);

    // claude's session works the desk of that one Senate, and its old
    // subagent's pin seats it as the harness's name beside it, not below.
    let claude = desk.inbox("s-claude");
    let start = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&start)
            .contains("[senator::claude] is a secretary of [senator::alice], the Speaker of"),
        "{}",
        context(&start)
    );
    let reviewer = desk.inbox("s-reviewer");
    let start = desk
        .session_start_as("s-reviewer", &reviewer, "claude/reviewer")
        .await
        .unwrap();
    assert!(!context(&start).contains("reviewer"), "{}", context(&start));
    desk.wait_connected("claude", true).await;
    let roster = top.daemon.hub.roster().await.unwrap();
    assert_eq!(roster.agents(), [path("claude")]);

    // The nested Senate's log is left as it was.
    assert_eq!(std::fs::read(under.join("log.db")).unwrap(), b"an old log");
    desk.stop().await;
}

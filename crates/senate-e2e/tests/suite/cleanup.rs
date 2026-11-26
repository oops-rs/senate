//! `senate cleanup` on a developer machine, against the real server's
//! rules: alice's machine hosts her Senate, she is its Speaker, and her
//! coding sessions come and go.
//!
//! The plan is asked for and applied as the CLI does it
//! (`Request::CleanupPlan`, then `Request::CleanupApply` carrying the plan
//! shown), so what the server refuses would be refused here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::*;
use senate_bridge::cleanup::{Cause, CommitteePlan, Plan, Report, Role, Scope};
use senate_bridge::client::ApiClient;
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::ipc::{self, JoinReply, Request, RosterCommittee};
use senate_bridge::session::SessionState;
use senate_bridge::state::Store;
use senate_core::alias::KnownAliases;
use senate_core::{
    Address, Event, Hub, Kind, Locale, NewEvent, Seq, StatusPayload, StatusState, meta,
};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

/// alice's own Senate, with her as its Speaker in person.
const HERS: &str = "[local]\n";

const SOCKET_ENV: &str = "SENATE_CLEANUP_AGENT_TEST_SOCKET";

/// Every dead senator, however lately it was active.
fn at_once() -> Scope {
    Scope {
        ended_for_secs: 0,
        ..Scope::default()
    }
}

fn of<'a>(plan: &'a Plan, committee: &senate_core::CommitteeId) -> &'a CommitteePlan {
    plan.committees
        .iter()
        .find(|c| &c.committee == committee)
        .unwrap()
}

fn names(seats: &[senate_bridge::cleanup::SeatPlan]) -> Vec<&str> {
    seats.iter().map(|seat| seat.senator.as_str()).collect()
}

async fn append(desk: &Desk, event: NewEvent) -> Event {
    desk.senate().daemon.hub.append(event).await.unwrap().event
}

/// How many events of the committee's log are what a cleanup writes: a
/// seat change or a status. (Presence comes and goes on its own.)
async fn written(desk: &Desk) -> usize {
    desk.events_of(&committee())
        .await
        .iter()
        .filter(|e| e.kind == Kind::Status || senate_core::CommitteeSeatChange::of(e).is_some())
        .count()
}

/// The status a cleanup wrote in the thread of `root`.
fn archive_of(events: &[Event], root: Seq) -> &Event {
    let statuses: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Status && e.thread == Some(root))
        .collect();
    assert_eq!(statuses.len(), 1, "one status in the thread of {root}");
    assert_eq!(
        StatusPayload::of(statuses[0]).unwrap().state,
        StatusState::Archived
    );
    statuses[0]
}

/// Where the Claude session `id` keeps its transcript: a file of the test.
fn transcript(desk: &Desk, id: &str) -> std::path::PathBuf {
    desk.home.join(format!("{id}.jsonl"))
}

/// The sessions History lists: (session, the name shown, its alias).
async fn history(desk: &Desk) -> Vec<(String, Option<String>, Option<String>)> {
    let page: ipc::SessionHistoryReply = desk
        .ipc(Request::SessionHistory {
            committee: None,
            before: None,
            limit: None,
        })
        .await;
    page.rows
        .into_iter()
        .map(|row| {
            (
                row.session_id,
                row.shown_name,
                row.alias.map(|alias| alias.to_string()),
            )
        })
        .collect()
}

/// A Claude Code session of its own identity (`claude-<id>`): it has an
/// inbox socket, a token, a process and a transcript on record, as a real
/// one does.
async fn claude(desk: &Desk, id: &str, inbox: &FakeInbox, kind: HookKind) {
    std::fs::write(transcript(desk, id), "").unwrap();
    let stdin = json!({
        "session_id": id,
        "cwd": desk.work,
        "transcript_path": transcript(desk, id),
        "hook_event_name": kind.event_name(),
        "source": "startup",
    })
    .to_string();
    let env = HookEnv {
        socket: Some(inbox.path.clone()),
        token: Some(format!("tok-{id}")),
        claude_pid: Some(std::process::id()),
        session_id: None,
        agent: None,
    };
    hook::run_as(
        kind,
        &desk.paths.socket(),
        &stdin,
        &env,
        None,
        HookTimeouts::default(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cleanup_removes_the_dead_and_only_what_waited_on_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    let codex = desk.codex_app_server();
    desk.start().await;
    // codex-live stays; codex-dead and codex-gone end. The first seated
    // chairs the committee, and it is the one that lives.
    for id in ["live", "dead", "gone"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
        desk.wait_connected(&format!("codex-{id}"), true).await;
    }
    let inbox = desk.inbox("c1");
    let claude_id = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let claude_name = format!("claude-{claude_id}");
    claude(&desk, claude_id, &inbox, HookKind::SessionStart).await;
    desk.wait_connected(&claude_name, true).await;
    for (id, alias) in [("live", "lead"), ("dead", "reviewer"), (claude_id, "docs")] {
        let reply: JoinReply = desk
            .ipc(Request::JoinAs {
                session: Laptop::session(id),
                committee: committee(),
                senator: path(alias),
            })
            .await;
        assert_eq!(reply.alias, Some(path(alias)));
    }
    let take_reviewer = Request::JoinAs {
        session: Laptop::session("live"),
        committee: committee(),
        senator: path("reviewer"),
    };
    // The alias is held: nobody else can take it.
    assert!(
        desk.try_ipc::<JoinReply>(take_reviewer.clone())
            .await
            .is_err()
    );

    let question = |from: Address, to: &[&Address], body: &str| {
        NewEvent::new(committee(), Kind::Question, from, body)
            .with_to(to.iter().map(|to| (*to).clone()).collect())
    };
    let dead = senator("codex-dead");
    let gone = senator("codex-gone");
    let live = senator("codex-live");
    // A person's question to a senator that will be gone.
    let waiting = append(&desk, question(person("alice"), &[&dead], "Which header?")).await;
    // One between two that will be gone, answered: settled, still open.
    let settled = append(&desk, question(gone.clone(), &[&dead], "Which branch?")).await;
    append(
        &desk,
        NewEvent::new(committee(), Kind::Answer, dead.clone(), "main")
            .in_thread(settled.seq)
            .to(gone.clone()),
    )
    .await;
    // What must stay: one to a live senator, one a live senator shares.
    let to_live = append(&desk, question(person("alice"), &[&live], "Still there?")).await;
    let shared = append(
        &desk,
        question(person("alice"), &[&dead, &live], "Either of you?"),
    )
    .await;
    // A permission notice of the senator that will be gone, written as its
    // bridge writes it, through the server's HTTP rules.
    let senate = desk.senate();
    let api = ApiClient::new(&senate.url, &senate.token).unwrap();
    let notice = api
        .append(
            &question(dead.clone(), &[&Address::Chair], "Tool approval is waiting")
                .with_meta(meta::HOSTED_APPROVAL, json!(true)),
        )
        .await
        .unwrap()
        .event;

    for id in ["dead", "gone"] {
        desk.codex_end(id).await;
        desk.wait_connected(&format!("codex-{id}"), false).await;
    }
    claude(&desk, claude_id, &inbox, HookKind::SessionEnd).await;
    desk.wait_connected(&claude_name, false).await;

    // They ended a moment ago: by default they are left, and so is
    // everything that waits on them.
    let plan: Plan = desk
        .ipc(Request::CleanupPlan {
            scope: Scope::default(),
        })
        .await;
    let ours = of(&plan, &committee());
    assert!(ours.seats.is_empty() && ours.threads.is_empty(), "{ours:?}");
    assert_eq!(
        names(&ours.recent),
        [claude_name.as_str(), "codex-dead", "codex-gone"]
    );
    assert!(plan.sessions.is_empty());
    assert!(plan.totals().is_empty());

    let plan: Plan = desk.ipc(Request::CleanupPlan { scope: at_once() }).await;
    let ours = of(&plan, &committee());
    assert_eq!(
        names(&ours.seats),
        [claude_name.as_str(), "codex-dead", "codex-gone"]
    );
    assert!(ours.roles.is_empty() && ours.recent.is_empty());
    let reviewer = &ours.seats[1];
    assert_eq!(reviewer.aliases, [path("reviewer")]);
    assert_eq!(reviewer.shown, "reviewer");
    assert!(reviewer.seated);
    assert_eq!(
        ours.threads
            .iter()
            .map(|t| (t.root, t.cause, t.notice, t.task))
            .collect::<Vec<_>>(),
        [
            (waiting.seq, Cause::AddresseeGone, false, true),
            (settled.seq, Cause::AllGone, false, true),
            (notice.seq, Cause::SessionEnded, true, false),
        ]
    );
    assert_eq!(ours.threads[2].id, notice.id);
    assert_eq!(ours.remaining.seats, 1);
    assert_eq!(ours.remaining.live, 1);
    assert_eq!(ours.remaining.chair.as_deref(), Some("lead"));
    assert!(ours.remaining.chair_live);
    // Only the Claude session left anything on this machine to forget.
    assert_eq!(
        plan.sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        [claude_id]
    );
    let totals = plan.totals();
    assert_eq!((totals.seats, totals.aliases, totals.threads()), (3, 2, 3));
    // History lists the ended Claude session, under a name it recorded.
    let listed = history(&desk).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, claude_id);

    let before = desk.events_of(&committee()).await;
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(plan.clone()),
        })
        .await;
    assert_eq!(report.failed, [], "{report:?}");
    assert_eq!(report.dropped, Vec::<String>::new());
    assert_eq!(report.new, 0);
    assert_eq!(report.sessions, 1);
    let done = &report.committees[0];
    assert_eq!((done.unseated, done.aliases), (3, 2));
    assert_eq!(
        done.threads,
        [
            (Cause::SessionEnded, 1),
            (Cause::AddresseeGone, 1),
            (Cause::AllGone, 1)
        ]
        .into()
    );

    // The seats are gone and the alias is free to give again.
    let roster: Vec<RosterCommittee> = desk.ipc(Request::Roster).await;
    let members: Vec<&str> = roster
        .iter()
        .find(|row| row.committee == committee())
        .unwrap()
        .members
        .iter()
        .map(|m| m.senator.as_str())
        .collect();
    assert_eq!(members, ["codex-live"]);
    let KnownAliases::Scoped(aliases) = desk.ipc(Request::Aliases).await else {
        panic!("a current server keeps aliases per committee");
    };
    assert_eq!(aliases.alias_of(&committee(), &path("codex-dead")), None);
    let reply: JoinReply = desk.ipc(take_reviewer).await;
    assert_eq!(reply.alias, Some(path("reviewer")));

    // Each thread was archived with its cause; nothing was removed.
    let after = desk.events_of(&committee()).await;
    let wrote = written(&desk).await;
    assert_eq!(after[..before.len()], before[..], "the log only grew");
    let said = archive_of(&after, waiting.seq);
    assert_eq!(said.from, person("alice"));
    assert_eq!(
        said.meta["cleanup"],
        json!({"cause": "addressee_gone", "by": "alice"})
    );
    assert!(StatusPayload::of(said).unwrap().reason.is_some());
    let said = archive_of(&after, settled.seq);
    assert_eq!(said.from, person("alice"));
    assert_eq!(said.meta["cleanup"]["cause"], "all_gone");
    // The notice is closed as a notice is: by the office in person, a bare
    // archive, the cause beside it.
    let said = archive_of(&after, notice.seq);
    assert_eq!(said.from, Address::Speaker);
    assert_eq!(said.meta[meta::DESK_PERSON], "alice");
    assert_eq!(said.meta[meta::APPROVAL_CLOSED], "session_ended");
    assert_eq!(said.meta[meta::HOSTED_APPROVAL], json!(notice.id));
    assert_eq!(said.meta["cleanup"]["cause"], "session_ended");
    assert!(
        !senate
            .daemon
            .hub
            .chair_docket(&committee())
            .await
            .unwrap()
            .items
            .iter()
            .any(|i| i.event.id == notice.id)
    );
    for kept in [to_live.seq, shared.seq] {
        assert!(
            !after
                .iter()
                .any(|e| e.kind == Kind::Status && e.thread == Some(kept)),
            "thread {kept} is a live senator's"
        );
    }

    // The session's record stays for History, with nothing to reach it by,
    // and History lists it exactly as before, though its alias is free now.
    let store = Store::open_read_only(&desk.paths.state_db()).unwrap();
    let record = store.session(claude_id).unwrap().unwrap();
    assert_eq!(record.state, SessionState::Ended);
    assert_eq!(record.agent, path(&claude_name));
    assert_eq!(
        record.transcript_path.as_deref(),
        transcript(&desk, claude_id).to_str()
    );
    assert!(record.socket.is_none() && record.token.is_none() && record.claude_pid.is_none());
    assert_eq!(aliases.alias_of(&committee(), &path(&claude_name)), None);
    assert_eq!(history(&desk).await, listed);

    // A second run finds nothing, and the plan approved before does nothing
    // twice: what it named no longer holds.
    let again: Plan = desk.ipc(Request::CleanupPlan { scope: at_once() }).await;
    assert!(again.totals().is_empty(), "{again:?}");
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(plan),
        })
        .await;
    assert_eq!(report.failed, []);
    assert_eq!(report.dropped.len(), 7, "{:?}", report.dropped);
    assert!(
        report
            .committees
            .iter()
            .all(|c| c.unseated + c.aliases == 0)
    );
    assert_eq!(report.sessions, 0);
    // Nothing more was written.
    assert_eq!(written(&desk).await, wrote);
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_chair_goes_only_when_asked_and_a_kept_senator_never() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    let codex = desk.codex_app_server();
    desk.start().await;
    // The first seated chairs the committee.
    for id in ["chair", "member", "keeper"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
        desk.wait_connected(&format!("codex-{id}"), true).await;
    }
    let hub = desk.senate().daemon.hub.clone();
    assert_eq!(
        hub.chair(&committee()).await.unwrap().chair,
        Some(path("codex-chair"))
    );
    for id in ["chair", "member", "keeper"] {
        desk.codex_end(id).await;
        desk.wait_connected(&format!("codex-{id}"), false).await;
    }
    let keep = |name: &str| Scope {
        keep: vec![path(name).into()],
        ..at_once()
    };
    // A name that finds nobody is refused: a typo must not unprotect.
    let refused = desk
        .try_ipc::<Plan>(Request::CleanupPlan {
            scope: keep("codex-keepr"),
        })
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("--keep codex-keepr"),
        "{refused}"
    );

    let plan: Plan = desk
        .ipc(Request::CleanupPlan {
            scope: keep("codex-keeper"),
        })
        .await;
    let ours = of(&plan, &committee());
    assert_eq!(names(&ours.seats), ["codex-member"]);
    assert_eq!(names(&ours.roles), ["codex-chair"]);
    assert_eq!(ours.roles[0].roles, [Role::Chair]);
    // The chair is listed, and not counted: this run leaves roles.
    assert_eq!(plan.totals().seats, 1);
    assert_eq!(ours.remaining.seats, 2);
    assert!(!ours.remaining.chair_live);
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(plan),
        })
        .await;
    assert_eq!(report.failed, []);
    assert_eq!(report.committees[0].unseated, 1);
    assert_eq!(
        hub.chair(&committee()).await.unwrap().chair,
        Some(path("codex-chair"))
    );

    let with_roles = Scope {
        include_roles: true,
        ..keep("codex-keeper")
    };
    let plan: Plan = desk.ipc(Request::CleanupPlan { scope: with_roles }).await;
    let ours = of(&plan, &committee());
    assert!(ours.seats.is_empty());
    assert_eq!(names(&ours.roles), ["codex-chair"]);
    assert_eq!(plan.totals().seats, 1);
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(plan),
        })
        .await;
    assert_eq!(report.failed, []);
    assert_eq!(report.committees[0].unseated, 1);
    // The kept senator still sits; the chair passed to it, the only seat
    // left, by the server's own rule.
    let roster: Vec<RosterCommittee> = desk.ipc(Request::Roster).await;
    let row = roster.iter().find(|r| r.committee == committee()).unwrap();
    assert_eq!(
        row.members
            .iter()
            .map(|m| m.senator.as_str())
            .collect::<Vec<_>>(),
        ["codex-keeper"]
    );
    assert_eq!(row.chair, Some(path("codex-keeper")));
    // The member unseated in the first run was never named chair.
    assert!(
        !desk
            .events_of(&committee())
            .await
            .iter()
            .any(|e| e.to.contains(&senator("codex-member")))
    );
    desk.stop().await;
}

/// The plan names every committee's senators and threads, and applying it
/// changes them: both are the person's. A process below the daemon (as an
/// agent's shell is below its session) is refused both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_can_neither_read_the_plan_nor_apply_one() {
    if let Some(socket) = std::env::var_os(SOCKET_ENV) {
        let call = |request: Request| {
            let socket = socket.clone();
            async move {
                ipc::request::<serde_json::Value>(
                    Path::new(&socket),
                    &request,
                    Duration::from_secs(2),
                    Duration::from_secs(10),
                )
                .await
                .unwrap_err()
            }
        };
        let plan = Plan {
            scope: at_once(),
            senate: "/alice".into(),
            person: name("alice"),
            epoch: "e".into(),
            derived_at: chrono::Utc::now(),
            committees: Vec::new(),
            sessions: Vec::new(),
        };
        for request in [
            Request::CleanupPlan { scope: at_once() },
            Request::CleanupApply {
                plan: Box::new(plan),
            },
        ] {
            let error = call(request.clone()).await;
            assert!(
                matches!(&error, ipc::ClientError::Remote(e) if e.code == ipc::codes::NOT_A_PERSON),
                "{request:?}: {error}"
            );
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    desk.start().await;
    let agent = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cleanup::an_agent_can_neither_read_the_plan_nor_apply_one",
            "--nocapture",
        ])
        .env(SOCKET_ENV, desk.paths.socket())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        agent.status.success(),
        "{}{}",
        String::from_utf8_lossy(&agent.stdout),
        String::from_utf8_lossy(&agent.stderr)
    );
    // The person, from their own side, reads it.
    let plan: Plan = desk.ipc(Request::CleanupPlan { scope: at_once() }).await;
    assert_eq!(plan.person, name("alice"));
    desk.stop().await;
}

/// On a team's Senate alice runs her agents and is not its Speaker: what
/// is hers to do is done, what only the Speaker's office may do is said
/// to be left, and a later run asks for nothing that was done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_is_refused_is_reported_and_the_rest_is_done() {
    let world = Setup::new()
        .without_speaker()
        .without_chat()
        .locale(Locale::En)
        .start()
        .await;
    let mut alice = world.laptop("alice");
    let codex = alice.codex_app_server();
    alice.open().await;
    for id in ["live", "dead"] {
        codex.idle(id);
        alice.codex_start_with_identity(id, true).await.unwrap();
        world.wait_connected(&format!("codex-{id}"), true).await;
    }
    let dead = senator("codex-dead");
    let hub = world.hub();
    let asked = hub
        .append(
            NewEvent::new(
                committee(),
                Kind::Question,
                person("alice"),
                "Which header?",
            )
            .to(dead.clone()),
        )
        .await
        .unwrap()
        .event;
    let notice = hub
        .append(
            NewEvent::new(
                committee(),
                Kind::Question,
                dead,
                "Tool approval is waiting",
            )
            .to(Address::Chair)
            .with_meta(meta::HOSTED_APPROVAL, json!(true)),
        )
        .await
        .unwrap()
        .event;
    alice.codex_end("dead").await;
    world.wait_connected("codex-dead", false).await;

    let plan: Plan = alice.ipc(Request::CleanupPlan { scope: at_once() }).await;
    let ours = of(&plan, &committee());
    assert_eq!(names(&ours.seats), ["codex-dead"]);
    assert_eq!(
        ours.threads
            .iter()
            .map(|t| (t.root, t.cause))
            .collect::<Vec<_>>(),
        [
            (asked.seq, Cause::AddresseeGone),
            (notice.seq, Cause::SessionEnded)
        ]
    );
    let report: Report = alice
        .ipc(Request::CleanupApply {
            plan: Box::new(plan),
        })
        .await;
    let done = &report.committees[0];
    assert_eq!(done.unseated, 1);
    assert_eq!(done.threads, [(Cause::AddresseeGone, 1)].into());
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(
        report.failed[0].what,
        format!("{} #{}", committee(), notice.seq)
    );
    assert!(
        report.failed[0].why.contains("Only the Speaker in person"),
        "{report:?}"
    );
    let events = world.events().await;
    assert_eq!(archive_of(&events, asked.seq).from, person("alice"));
    assert!(
        !events
            .iter()
            .any(|e| e.kind == Kind::Status && e.thread == Some(notice.seq))
    );
    // What was done is not asked for again; what was refused still shows.
    let again: Plan = alice.ipc(Request::CleanupPlan { scope: at_once() }).await;
    let ours = of(&again, &committee());
    assert!(ours.seats.is_empty(), "{ours:?}");
    assert_eq!(
        ours.threads.iter().map(|t| t.root).collect::<Vec<_>>(),
        [notice.seq]
    );
    alice.close().await;
    world.stop().await;
}

/// `senate cleanup …` as a person runs it from their own terminal: not
/// below the test's process, where the daemon lives (its guard refuses a
/// caller there, as the test above shows). The outer shell exits at once,
/// so the command has no such parent by the time it connects. Its stdout
/// is a file, not a terminal.
fn senate_cleanup(desk: &Desk, root: &Path, args: &[&str]) -> (bool, String, String) {
    let run = tempfile::tempdir_in(root).unwrap();
    let mut cmd = std::process::Command::new("/bin/sh");
    senate_bridge::testenv::isolate(&mut cmd, root);
    let status = cmd
        .env("HOME", root)
        .env("SENATE_HOME", desk.paths.home())
        .current_dir(run.path())
        .arg("-c")
        .arg(r#"( sleep 0.3; "$0" cleanup "$@" > out 2> err; echo $? > code.tmp; mv code.tmp code ) < /dev/null > /dev/null 2>&1 &"#)
        .arg(env!("CARGO_BIN_EXE_senate-e2e-cli"))
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
    let code = run.path().join("code");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !code.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "senate cleanup {args:?} never finished"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let read = |name: &str| std::fs::read_to_string(run.path().join(name)).unwrap();
    (read("code").trim() == "0", read("out"), read("err"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_shows_the_plan_and_changes_nothing_until_told() {
    // Sockets of the command and its daemon sit below this root.
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    let codex = desk.codex_app_server();
    desk.start().await;
    for id in ["live", "dead"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
        desk.wait_connected(&format!("codex-{id}"), true).await;
    }
    let _: JoinReply = desk
        .ipc(Request::JoinAs {
            session: Laptop::session("dead"),
            committee: committee(),
            senator: path("reviewer"),
        })
        .await;
    desk.codex_end("dead").await;
    desk.wait_connected("codex-dead", false).await;
    // Asked once it was gone: nothing of it is queued on this machine.
    let asked = append(
        &desk,
        NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Which header?",
        )
        .to(senator("codex-dead")),
    )
    .await;
    let events = written(&desk).await;
    let run = |args: &'static [&'static str]| {
        let (ok, out, err) =
            tokio::task::block_in_place(|| senate_cleanup(&desk, dir.path(), args));
        assert!(ok, "senate cleanup {args:?}\n{out}\n{err}");
        out
    };

    // It ended a moment ago: left, and said so with what would take it.
    let out = run(&["--dry-run"]);
    assert!(out.contains("Nothing to clean up."), "{out}");
    assert!(
        out.contains("active within 1h, left this time (--ended-for 0s takes them) 1: reviewer"),
        "{out}"
    );

    let plan = run(&["--dry-run", "--ended-for", "0s"]);
    for line in [
        "refund-v2  (1 seat(s) remain, 1 live; chair codex-live is live)",
        "  unseat 1: reviewer",
        "  free aliases 1: reviewer",
        "  archive threads 1: 1 addressee_gone",
        &format!("    #{} addressee_gone · question · alice · ", asked.seq),
        "In all: 1 senator(s); unseat 1, free 1 alias(es), drop 0 queued message(s), archive 1 thread(s) (1 addressee_gone), forget 0 session record(s)",
        "Kept: the message log, session history",
    ] {
        assert!(plan.contains(line), "missing {line:?} in\n{plan}");
    }
    // Without a terminal to ask on and without --yes: the plan, nothing more.
    let out = run(&["--ended-for", "0s"]);
    assert!(out.contains("  unseat 1: reviewer"), "{out}");
    assert!(out.contains("nothing was changed; add --yes"), "{out}");
    assert_eq!(written(&desk).await, events);

    let out = run(&["--ended-for", "0s", "--yes"]);
    assert!(
        out.contains(
            "Done: unseated 1, freed 1 alias(es), dropped 0 queued message(s), archived 1 thread(s) (1 addressee_gone), forgot 0 session record(s)."
        ),
        "{out}"
    );
    // One seat removed, one thread archived.
    assert_eq!(written(&desk).await, events + 2);
    let out = run(&["--ended-for", "0s", "--yes"]);
    assert!(out.contains("Nothing to clean up."), "{out}");
    assert_eq!(written(&desk).await, events + 2);

    // A name --keep cannot find fails the command, naming it.
    let (ok, _, err) = tokio::task::block_in_place(|| {
        senate_cleanup(&desk, dir.path(), &["--dry-run", "--keep", "nobody"])
    });
    assert!(!ok && err.contains("--keep nobody"), "{err}");
    desk.stop().await;
}

/// One line a committee: what a plan would do there, in counts.
fn summary(plan: &Plan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for c in &plan.committees {
        if c.is_empty() {
            continue;
        }
        let acted: Vec<_> = plan.acted(c).collect();
        let mut causes = std::collections::BTreeMap::new();
        for thread in &c.threads {
            *causes.entry(thread.cause.as_str()).or_insert(0) += 1;
        }
        let _ = writeln!(
            out,
            "{:<44} unseat {:>3}  aliases {:>3}  queued {:>3}  followed {:>3}  threads {:>3} {:?}  roles {:>2}  recent {:>2}  left {:>2} seats / {:>2} live, chair live {}{}",
            c.committee.as_str(),
            acted.iter().filter(|s| s.seated).count(),
            acted.iter().map(|s| s.aliases.len()).sum::<usize>(),
            acted.iter().map(|s| s.deliveries).sum::<usize>(),
            acted.iter().filter(|s| s.followed).count(),
            c.threads.len(),
            causes,
            c.roles.len(),
            c.recent.len(),
            c.remaining.seats,
            c.remaining.live,
            c.remaining.chair_live,
            if c.archived { "  (archived)" } else { "" },
        );
    }
    let totals = plan.totals();
    let _ = writeln!(
        out,
        "TOTAL senators {}  seats {}  aliases {}  queued {}  threads {} {:?}  sessions {} ({} hosted, {} stale)  roles listed {}  recent {}",
        totals.senators,
        totals.seats,
        totals.aliases,
        totals.deliveries,
        totals.threads(),
        totals
            .threads
            .iter()
            .map(|(cause, n)| (cause.as_str(), *n))
            .collect::<Vec<_>>(),
        totals.sessions,
        plan.sessions.iter().filter(|s| s.hosted).count(),
        plan.sessions.iter().filter(|s| s.stale).count(),
        plan.committees.iter().map(|c| c.roles.len()).sum::<usize>(),
        plan.committees
            .iter()
            .map(|c| c.recent.len())
            .sum::<usize>(),
    );
    out
}

/// A rehearsal on a copy of a real Senate home, never the home itself.
/// Not a test of the code: it prints what `senate cleanup` would do to
/// that home, applies it to the copy, and says what the server refused.
///
/// `SENATE_CLEANUP_REHEARSAL` names a directory prepared by hand:
///
/// - `state.db` and `log.db`: copies made with `sqlite3 <src> ".backup
///   <dst>"`. In the copy of `state.db`, every inbox socket is replaced by
///   a path that does not exist (so nothing is pushed to a real session)
///   and `epochs.server` is the address in `local.toml`.
/// - `person`: the person's handle. `local.toml`: the `[local]` table,
///   with a `listen` address of its own.
/// - `running-hosts`: the host id of each hosted session that was running,
///   one a line. An empty `owner` lock is held for each, so the copy sees
///   those sessions live as the real daemon does. No pid is written there:
///   a host with none recorded is killed by nothing.
///
/// `SENATE_CLEANUP_REHEARSAL_HOME` is the short directory under `/tmp` the
/// copy's home is made in; run it with `SENATE_CONFINE_TO` set to the same
/// directory, so no socket outside it is ever connected to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a rehearsal on a hand-prepared copy of a real Senate home"]
async fn rehearsal_on_a_copy_of_a_real_home() {
    let (Some(source), Some(root)) = (
        std::env::var_os("SENATE_CLEANUP_REHEARSAL"),
        std::env::var_os("SENATE_CLEANUP_REHEARSAL_HOME"),
    ) else {
        return;
    };
    let (source, root) = (Path::new(&source), Path::new(&root));
    assert_eq!(
        std::env::var_os(senate_bridge::testenv::CONFINE_ENV).as_deref(),
        Some(root.as_os_str()),
        "confine the rehearsal to its own home"
    );
    let read = |name: &str| std::fs::read_to_string(source.join(name)).unwrap();
    let who = read("person").trim().to_owned();
    let mut desk = Desk::new(root, &who, Locale::En, &read("local.toml"));
    std::fs::copy(source.join("state.db"), desk.paths.state_db()).unwrap();
    let top = desk.paths.hosted("top");
    std::fs::create_dir_all(&top).unwrap();
    std::fs::copy(source.join("log.db"), top.join("log.db")).unwrap();
    let mut locks = Vec::new();
    for host in read("running-hosts")
        .lines()
        .filter(|line| !line.is_empty())
    {
        let dir = desk.paths.home().join("hosts").join(host);
        std::fs::create_dir_all(&dir).unwrap();
        let owner = std::fs::File::create(dir.join("owner")).unwrap();
        owner.try_lock().unwrap();
        locks.push(owner);
    }
    desk.start().await;

    let records = || {
        Store::open_read_only(&desk.paths.state_db())
            .unwrap()
            .all_sessions()
            .unwrap()
            .len()
    };
    let history = || async {
        let mut rows = 0;
        let mut before = None;
        loop {
            // The first listing reads every transcript's title.
            let page: ipc::SessionHistoryReply = ipc::request(
                &desk.paths.socket(),
                &Request::SessionHistory {
                    committee: None,
                    before,
                    limit: Some(300),
                },
                Duration::from_secs(2),
                Duration::from_secs(600),
            )
            .await
            .unwrap();
            rows += page.rows.len();
            before = page.next_before;
            if before.is_none() {
                return rows;
            }
        }
    };
    let day = Some(24 * 60 * 60);
    let scopes = [
        ("default (ended-for 1h)", Scope::default()),
        ("--ended-for 0s", at_once()),
        (
            "--include-roles",
            Scope {
                include_roles: true,
                ..Scope::default()
            },
        ),
        (
            "--older-than 1d",
            Scope {
                older_than_secs: day,
                ..Scope::default()
            },
        ),
        (
            "--ended-for 0s --include-roles --older-than 1d",
            Scope {
                include_roles: true,
                older_than_secs: day,
                ..at_once()
            },
        ),
    ];
    let mut plans = Vec::new();
    for (label, scope) in scopes {
        let started = std::time::Instant::now();
        let plan: Plan = desk.ipc(Request::CleanupPlan { scope }).await;
        println!(
            "\n== plan: {label}  (derived in {:?}, {} bytes as a request)\n{}",
            started.elapsed(),
            serde_json::to_vec(&plan).unwrap().len(),
            summary(&plan)
        );
        plans.push((label, plan));
    }
    std::fs::write(
        source.join("plans.json"),
        serde_json::to_vec_pretty(&plans).unwrap(),
    )
    .unwrap();

    // Apply the first run a person would make, then the widest, each to
    // the copy, and say what was done, left and refused.
    let (records_before, history_before) = (records(), history().await);
    for at in [0, plans.len() - 1] {
        let (label, _) = &plans[at];
        let plan: Plan = desk
            .ipc(Request::CleanupPlan {
                scope: plans[at].1.scope.clone(),
            })
            .await;
        let report: Report = ipc::request(
            &desk.paths.socket(),
            &Request::CleanupApply {
                plan: Box::new(plan.clone()),
            },
            Duration::from_secs(2),
            Duration::from_secs(600),
        )
        .await
        .unwrap();
        let mut done = senate_bridge::cleanup::CommitteeDone::default();
        for c in &report.committees {
            done.unseated += c.unseated;
            done.aliases += c.aliases;
            done.deliveries += c.deliveries;
            for (cause, n) in &c.threads {
                *done.threads.entry(*cause).or_default() += n;
            }
        }
        println!(
            "\n== applied to the copy: {label}\nunseated {}  aliases {}  queued dropped {}  threads {:?}  sessions forgotten {}\nfailed {}  dropped {}  new {}",
            done.unseated,
            done.aliases,
            done.deliveries,
            done.threads
                .iter()
                .map(|(cause, n)| (cause.as_str(), *n))
                .collect::<Vec<_>>(),
            report.sessions,
            report.failed.len(),
            report.dropped.len(),
            report.new,
        );
        for failed in &report.failed {
            println!("  REFUSED {}: {}", failed.what, failed.why);
        }
        let again: Plan = desk
            .ipc(Request::CleanupPlan {
                scope: plan.scope.clone(),
            })
            .await;
        println!(
            "second run of the same: {}",
            if again.totals().is_empty() {
                "nothing left".to_owned()
            } else {
                summary(&again)
            }
        );
        println!(
            "session records {} -> {}, History rows {} -> {}",
            records_before,
            records(),
            history_before,
            history().await
        );
    }
    desk.stop().await;
    drop(locks);
}

/// An archived committee takes no new seat or thread, and its seats can
/// still be removed and its threads closed: a cleanup reads it and cleans
/// it like any other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archived_committee_is_cleaned_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    let codex = desk.codex_app_server();
    desk.start().await;
    for id in ["first", "dead"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
        desk.wait_connected(&format!("codex-{id}"), true).await;
    }
    for id in ["first", "dead"] {
        desk.codex_end(id).await;
        desk.wait_connected(&format!("codex-{id}"), false).await;
    }
    let asked = append(
        &desk,
        NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Which header?",
        )
        .to(senator("codex-dead")),
    )
    .await;
    let archived: senate_core::api::CommitteeArchiveInfo = desk
        .ipc(Request::ArchiveCommittee {
            committee: committee(),
            archived: true,
            id: None,
        })
        .await;
    assert!(archived.archived);

    let scope = Scope {
        include_roles: true,
        ..at_once()
    };
    let plan: Plan = desk.ipc(Request::CleanupPlan { scope }).await;
    let ours = of(&plan, &committee());
    assert!(ours.archived);
    assert_eq!(names(&ours.seats), ["codex-dead"]);
    assert_eq!(names(&ours.roles), ["codex-first"]);
    assert_eq!(
        ours.threads
            .iter()
            .map(|t| (t.root, t.cause))
            .collect::<Vec<_>>(),
        [(asked.seq, Cause::AddresseeGone)]
    );
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(plan.clone()),
        })
        .await;
    assert_eq!(report.failed, [], "{report:?}");
    assert_eq!(report.committees[0].unseated, 2);
    assert_eq!(
        report.committees[0].threads,
        [(Cause::AddresseeGone, 1)].into()
    );
    let again: Plan = desk.ipc(Request::CleanupPlan { scope: plan.scope }).await;
    assert!(again.totals().is_empty(), "{again:?}");
    desk.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_made_for_something_else_is_refused_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(dir.path(), "alice", Locale::En, HERS);
    let codex = desk.codex_app_server();
    desk.start().await;
    for id in ["live", "dead"] {
        codex.idle(id);
        desk.codex_start_with_identity(id, true).await.unwrap();
        desk.wait_connected(&format!("codex-{id}"), true).await;
    }
    let asked = append(
        &desk,
        NewEvent::new(
            committee(),
            Kind::Question,
            person("alice"),
            "Which header?",
        )
        .to(senator("codex-dead")),
    )
    .await;
    desk.codex_end("dead").await;
    desk.wait_connected("codex-dead", false).await;
    let plan: Plan = desk.ipc(Request::CleanupPlan { scope: at_once() }).await;
    assert_eq!(of(&plan, &committee()).threads[0].root, asked.seq);
    let events = written(&desk).await;

    let refused = |plan: Plan| async {
        desk.try_ipc::<Report>(Request::CleanupApply {
            plan: Box::new(plan),
        })
        .await
        .unwrap_err()
        .to_string()
    };
    // Another log, another person, an old plan.
    let mut other = plan.clone();
    other.epoch = "another-epoch".into();
    assert!(
        refused(other)
            .await
            .contains("another Senate, person or log")
    );
    let mut other = plan.clone();
    other.person = name("bob");
    assert!(
        refused(other)
            .await
            .contains("another Senate, person or log")
    );
    let mut old = plan.clone();
    old.derived_at -= chrono::Duration::minutes(16);
    assert!(refused(old).await.contains("too old"));
    // What no plan of this Senate could name: a thread whose root is
    // another event, a senator that is no session's, a committee unknown.
    let mut forged = plan.clone();
    forged.committees[0].threads[0].id = senate_core::EventId::new();
    assert!(
        refused(forged)
            .await
            .contains("not a cleanup plan of this Senate")
    );
    let mut forged = plan.clone();
    forged.committees[0].seats[0].senator = path("alice");
    assert!(
        refused(forged)
            .await
            .contains("not a cleanup plan of this Senate")
    );
    let mut forged = plan.clone();
    forged.committees[0].committee = "nowhere".parse().unwrap();
    assert!(
        refused(forged)
            .await
            .contains("not a cleanup plan of this Senate")
    );
    // Nothing was done by any of them.
    assert_eq!(written(&desk).await, events);

    // A live senator slipped into an approved plan is drift, not a plan
    // of something else: it is left, and the rest is done.
    let mut padded = plan.clone();
    let mut seat = padded.committees[0].seats[0].clone();
    seat.senator = path("codex-live");
    seat.shown = "codex-live".into();
    padded.committees[0].seats.push(seat);
    let report: Report = desk
        .ipc(Request::CleanupApply {
            plan: Box::new(padded),
        })
        .await;
    assert_eq!(report.failed, []);
    assert_eq!(report.dropped, ["refund-v2: codex-live"]);
    assert_eq!(report.committees[0].unseated, 1);
    desk.wait_connected("codex-live", true).await;
    desk.stop().await;
}

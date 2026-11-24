//! Statistics through the real bridge socket and CLI, from synthetic transcripts.

#![allow(clippy::unwrap_used)]
#[allow(dead_code)]
#[path = "support/agy_timeline.rs"]
mod agy;
#[path = "support/common.rs"]
mod common;

use std::{io::Write, path::Path, time::Duration};

use chrono::{DateTime, Utc};
use senate_bridge::{
    agent::AgentSource,
    ipc::{self, Request},
    session::{CommitteeSource, Runtime},
    state::SessionUpsert,
    stats::StatsReply,
};
use senate_core::{Address, Kind, NewEvent};
use serde_json::{Value, json};

fn row(h: &common::Harness, id: &str, runtime: Runtime, path: &Path) {
    senate_bridge::state::Store::open(&h.paths.state_db())
        .unwrap()
        .upsert_session(&SessionUpsert {
            session_id: id.into(),
            cwd: h.dir.path().join("repo-fixture"),
            git_root: Some(h.dir.path().join("repo-fixture")),
            branch: None,
            inferred_committee: Some((common::committee(), CommitteeSource::Join)),
            socket: None,
            token: None,
            claude_pid: None,
            transcript_path: Some(path.display().to_string()),
            ancestor_pids: vec![],
            runtime,
            agent: common::path(if runtime == Runtime::Agy {
                "agy-fixture"
            } else {
                id
            }),
            agent_source: AgentSource::Session,
        })
        .unwrap();
}

fn append(path: &Path, at: DateTime<Utc>, mut record: Value) {
    record["timestamp"] = json!(at);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    writeln!(file, "{record}").unwrap();
}

async fn settled(h: &common::Harness, request: &Request, prompts: u64) -> StatsReply {
    let timeout = Duration::from_secs(5);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let reply: StatsReply = ipc::request(&h.socket(), request, timeout, timeout)
                .await
                .unwrap();
            if !reply.indexing && reply.totals.prompts == prompts {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_roll_up_three_runtimes_and_refresh_when_a_transcript_grows() {
    let mut h = common::Harness::new().await;
    h.paths.ensure_home().unwrap();
    let today = Utc::now().date_naive();
    let first = today - chrono::Duration::days(2);
    let second = today - chrono::Duration::days(1);
    let a = first.and_hms_opt(23, 58, 0).unwrap().and_utc();
    let b = second.and_hms_opt(0, 1, 0).unwrap().and_utc();
    let c = second.and_hms_opt(10, 0, 0).unwrap().and_utc();
    let claude = h.dir.path().join("claude.jsonl");
    append(
        &claude,
        a,
        json!({"type":"user","message":{"content":"fixture prompt"}}),
    );
    append(
        &claude,
        b,
        json!({"type":"assistant","message":{
            "id":"msg-1","usage":{"input_tokens":12,"output_tokens":6,"cache_read_input_tokens":8},
            "content":[{"type":"text","text":"fixture reply"},{"type":"tool_use","id":"tool-1","name":"Bash","input":{"command":"fixture command"}}]
        }}),
    );
    append(
        &claude,
        b,
        json!({"type":"assistant","message":{
            "id":"msg-1","usage":{"input_tokens":12,"output_tokens":9,"cache_read_input_tokens":8},
            "content":[{"type":"tool_use","id":"tool-2","name":"mcp__fixture__call","input":{}}]
        }}),
    );
    row(&h, "claude-fixture", Runtime::Claude, &claude);

    let codex = h.dir.path().join("codex.jsonl");
    let message = |role: &str, kind: &str| {
        json!({"type":"response_item","payload":{"type":"message","role":role,
            "content":[{"type":kind,"text":"fixture turn"}]}})
    };
    append(&codex, c, message("user", "input_text"));
    append(
        &codex,
        c + chrono::Duration::minutes(7),
        message("assistant", "output_text"),
    );
    for (input, output) in [(100, 20), (100, 20), (150, 35)] {
        append(
            &codex,
            c,
            json!({"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"input_tokens":input,"output_tokens":output}
            }}}),
        );
    }
    row(&h, "codex-fixture", Runtime::Codex, &codex);

    let agy_path = h.dir.path().join(format!("{}.db", agy::ID));
    {
        use agy::{field, number};
        let db = agy::create(&agy_path);
        agy::user(&db, 0, 4, "fixture");
        agy::planner(&db, 1, "fixture reply", "");
        let meta = |at: DateTime<Utc>| field(1, number(1, at.timestamp() as u64));
        // Preserve the provider source enum on the prompt record.
        db.execute(
            "UPDATE steps SET metadata=?1 WHERE idx=0",
            [[meta(c), number(3, 4)].concat()],
        )
        .unwrap();
        db.execute("UPDATE steps SET metadata=?1 WHERE idx=1", [meta(c)])
            .unwrap();
        let counts = [
            number(2, 50),
            number(3, 8),
            number(5, 4),
            number(6, 24),
            field(11, "generation"),
        ]
        .concat();
        let generation = [field(1, field(4, counts)), number(2, 0)].concat();
        db.execute(
            "INSERT INTO gen_metadata(idx,data) VALUES(0,?1)",
            [generation],
        )
        .unwrap();
    }
    row(&h, agy::SESSION, Runtime::Agy, &agy_path);
    h.server.append(
        NewEvent::new(
            common::committee(),
            Kind::Note,
            Address::senior(&h.config.person),
            "private fixture committee note",
        )
        .to(Address::senator(&common::path("claude-fixture"))),
    );
    h.server.append(
        NewEvent::new(
            common::committee(),
            Kind::Question,
            Address::senator(&common::path("fixture-agent")),
            "private received question",
        )
        .to(Address::senior(&h.config.person)),
    );
    h.start().await;

    let range = Request::Stats {
        from: first,
        to: today,
        tz: "UTC".into(),
    };
    let report = settled(&h, &range, 3).await;
    assert_eq!(report.days[0].counts.active_seconds, 120);
    assert_eq!(report.days[1].counts.active_seconds, 60);
    assert_eq!(report.days[0].by_runtime["claude"].sessions_active, 1);
    assert_eq!(report.days[1].by_runtime.len(), 3);
    assert_eq!(report.totals.sessions_started, 3);
    assert_eq!(report.totals.agent_turns, 3);
    assert_eq!(report.totals.commands, 1);
    assert_eq!(report.totals.mcp_calls, 1);
    assert_eq!(report.totals.sent["note"], 1);
    assert_eq!(report.totals.received["question"], 1);
    assert_eq!(report.totals.tokens["claude"].input, 20);
    assert_eq!(report.totals.tokens["claude"].output, 9);
    assert_eq!(report.totals.tokens["codex"].input, 150);
    assert_eq!(report.totals.tokens["codex"].output, 35);
    assert_eq!(report.totals.tokens["agy"].input, 54);
    assert_eq!(report.totals.tokens["agy"].output, 8);
    assert_eq!(report.longest_streak, 3);
    assert_eq!(report.top_repositories[0].name, "repo-fixture");
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("private fixture committee note"));
    assert!(!json.contains(h.dir.path().to_str().unwrap()));

    append(
        &claude,
        a,
        json!({"type":"user","message":{"content":"a later record for an older day"}}),
    );
    let refreshed = settled(&h, &range, 4).await;
    assert_eq!(refreshed.days[0].counts.prompts, 2);

    let taipei = settled(
        &h,
        &Request::Stats {
            from: first,
            to: today,
            tz: "Asia/Taipei".into(),
        },
        4,
    )
    .await;
    assert_eq!(taipei.days[0].counts.active_seconds, 0);
    assert_eq!(taipei.days[1].counts.active_seconds, 180);
    assert_eq!(
        taipei.totals.active_seconds,
        refreshed.totals.active_seconds
    );
    assert_eq!(taipei.totals.prompts, refreshed.totals.prompts);
    assert_eq!(taipei.totals.tokens, refreshed.totals.tokens);

    let year = Request::Stats {
        from: today - chrono::Duration::days(364),
        to: today,
        tz: "UTC".into(),
    };
    let yearly = settled(&h, &year, 4).await;
    assert_eq!(yearly.days.len(), 365);
    let start = std::time::Instant::now();
    let read: StatsReply = ipc::request(
        &h.socket(),
        &year,
        Duration::from_secs(2),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    eprintln!("warm 365-day stats IPC: {:.2?}", start.elapsed());
    assert_eq!(read.totals, yearly.totals);
    assert!(!read.indexing);
}

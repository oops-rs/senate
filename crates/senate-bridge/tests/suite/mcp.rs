//! The MCP server end to end: JSON-RPC in, IPC to a real daemon, HTTP to
//! the fake server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::*;
use rmcp::ServiceExt;
use senate_bridge::hook::{HookEnv, HookKind};
use senate_bridge::ipc::{self, ClientError, Posted, Request, SessionSelector};
use senate_bridge::mcp::SenateMcp;
use senate_bridge::session::{CommitteeSource, Runtime};
use senate_bridge::state::ItemStatus;
use senate_core::{
    Address, EventId, Kind, Locale, NewEvent, PROPOSAL_DECISION, Seq, StatusPayload, StatusState,
    meta,
};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

struct McpClient {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    next_id: u64,
    /// `notifications/tools/list_changed` read so far.
    list_changes: usize,
}

impl McpClient {
    async fn start(daemon_socket: PathBuf, selector: SessionSelector) -> Self {
        Self::start_in(daemon_socket, selector, Locale::En).await
    }

    /// A shim writing its tool results in `locale`.
    async fn start_in(daemon_socket: PathBuf, selector: SessionSelector, locale: Locale) -> Self {
        Self::start_for(daemon_socket, selector, locale, Runtime::Claude).await
    }

    async fn start_for(
        daemon_socket: PathBuf,
        selector: SessionSelector,
        locale: Locale,
        runtime: Runtime,
    ) -> Self {
        Self::start_named(daemon_socket, selector, locale, runtime, "test").await
    }

    /// As Codex connects: it names itself `codex-mcp-client`.
    async fn start_codex(daemon_socket: PathBuf, selector: SessionSelector) -> Self {
        Self::start_named(
            daemon_socket,
            selector,
            Locale::En,
            Runtime::Claude,
            "codex-mcp-client",
        )
        .await
    }

    async fn start_named(
        daemon_socket: PathBuf,
        selector: SessionSelector,
        locale: Locale,
        runtime: Runtime,
        client_name: &str,
    ) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            // As `senate mcp` serves: the server, and beside it what tells
            // the client when the session's tools change.
            let mcp = SenateMcp::new(daemon_socket, selector)
                .with_locale(locale)
                .with_pins(senate_bridge::mcp::Pins {
                    runtime,
                    ..Default::default()
                });
            let running = mcp.clone().serve(server).await.unwrap();
            let roles = tokio::spawn(mcp.follow_roles(running.peer().clone()));
            let _ = running.waiting().await;
            roles.abort();
        });
        let (r, w) = tokio::io::split(client);
        let mut c = Self {
            lines: BufReader::new(r).lines(),
            writer: w,
            next_id: 1,
            list_changes: 0,
        };
        let init = c
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": client_name, "version": "0"}
                }),
            )
            .await;
        assert_eq!(init["serverInfo"]["name"], "senate");
        assert_eq!(
            init["capabilities"]["tools"]["listChanged"], true,
            "the tools change with what the session's agent is"
        );
        assert!(
            init["instructions"]
                .as_str()
                .unwrap()
                .contains("not from your user")
        );
        c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        c
    }

    /// Connect using MCP 2026-07-28 stateless discovery (`server/discover`
    /// with per-request `_meta`, no `initialize` handshake), as Codex 0.172+
    /// negotiates with rmcp 3.x.
    async fn start_discover(
        daemon_socket: PathBuf,
        selector: SessionSelector,
        runtime: Runtime,
        client_name: Option<&str>,
    ) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let mcp = SenateMcp::new(daemon_socket, selector)
                .with_locale(Locale::En)
                .with_pins(senate_bridge::mcp::Pins {
                    runtime,
                    ..Default::default()
                });
            let running = mcp.clone().serve(server).await.unwrap();
            let roles = tokio::spawn(mcp.follow_roles(running.peer().clone()));
            let _ = running.waiting().await;
            roles.abort();
        });
        let (r, w) = tokio::io::split(client);
        let mut c = Self {
            lines: BufReader::new(r).lines(),
            writer: w,
            next_id: 1,
            list_changes: 0,
        };
        let discover = c
            .request(
                "server/discover",
                json!({ "_meta": discover_meta(client_name) }),
            )
            .await;
        assert!(
            discover["supportedVersions"]
                .as_array()
                .is_some_and(|v| v.iter().any(|ver| ver == "2026-07-28")),
            "{discover}"
        );
        c
    }

    async fn send(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let v = self.rpc(method, params).await;
        assert!(v.get("error").is_none(), "rpc error: {v}");
        v["result"].clone()
    }

    /// The whole JSON-RPC response, error or result.
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("MCP reply in time")
                .unwrap()
                .expect("MCP stream open");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            if v["method"] == "notifications/tools/list_changed" {
                self.list_changes += 1;
            }
        }
    }

    /// Wait for the server to say the tool list changed.
    async fn list_changed(&mut self) {
        loop {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("the tool list to be said changed in time")
                .unwrap()
                .expect("MCP stream open");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["method"] == "notifications/tools/list_changed" {
                self.list_changes += 1;
                return;
            }
        }
    }

    /// The names of the tools the server lists now.
    async fn tool_names(&mut self) -> Vec<String> {
        self.request("tools/list", json!({})).await["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// `(is_error, text)`.
    async fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let r = self
            .request("tools/call", json!({"name": tool, "arguments": args}))
            .await;
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (r["isError"].as_bool().unwrap_or(false), text)
    }
}

fn selector(id: &str) -> SessionSelector {
    SessionSelector {
        session_id: Some(id.into()),
        claude_pid: None,
    }
}

fn git_repo(dir: &Path, branch: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let status = Command::new("git")
        .args(["init", "-q", "-b", branch])
        .arg(dir)
        .status()
        .unwrap();
    assert!(status.success());
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in:\n{text}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chair_discovers_and_spawns_a_profile_with_existing_role_checks() {
    use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
    use senate_bridge::ipc::{HookReply, SpawnReply};

    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("project", Some(committee().as_str()));
    let fixture = agy_fixture(h.dir.path(), "8b135e59-a160-4ed4-a0e9-105ed957be07");
    // The daemon starts with no profile: they are added while it runs.
    let config = format!(
        "{}\n[agy]\nbin = {fixture:?}\n",
        toml::to_string(&h.config).unwrap()
    );
    std::fs::write(h.paths.config(), &config).unwrap();
    h.config = senate_bridge::BridgeConfig::load(&h.paths.config()).unwrap();
    h.config.committees.push(committee());
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            env!("CARGO_BIN_EXE_senate").into(),
        )
        .await
        .unwrap(),
    );
    let hook = serde_json::from_value(json!({
        "event":"session_start", "runtime":"claude", "session_id":"chair-fixture",
        "cwd":project, "committee_file":committee(), "source":"startup",
    }))
    .unwrap();
    let _: HookReply = ipc::request(&h.socket(), &Request::SessionHook(hook), WAIT, WAIT)
        .await
        .unwrap();
    h.server.state.inner.lock().unwrap().chair = Some((path("claude-chair-fixture"), true));
    let mut chair = McpClient::start(h.socket(), selector("chair-fixture")).await;
    let tools = chair.request("tools/list", json!({})).await;
    let spawn = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "spawn")
        .unwrap();
    let schema = spawn["inputSchema"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["properties"].get("runtime").is_some())
        .unwrap();
    assert!(schema["properties"].get("profile").is_some(), "{schema}");
    assert!(schema["properties"].get("model").is_none(), "{schema}");
    assert!(schema["properties"].get("effort").is_none(), "{schema}");
    assert!(schema["properties"].get("speed").is_none(), "{schema}");
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "launch_profiles")
    );
    // With none, the tool says how one is added and where.
    let (error, none) = chair.call("launch_profiles", json!({})).await;
    assert!(!error, "{none}");
    assert!(
        none.starts_with("No launch profiles are configured"),
        "{none}"
    );
    assert!(none.contains("[profiles.review]\nruntime = \"codex\""));
    assert!(
        none.contains(&h.paths.config().display().to_string()),
        "{none}"
    );
    assert!(none.contains("no restart"), "{none}");

    // Added to the file, they are listed and usable with the daemon as it is.
    let triage = "[profiles.triage]\nruntime = \"agy\"\nmodel = \"flash\"\neffort = \"high\"\nnote = \"Use for test triage.\"\n[profiles.quick]\nruntime = \"codex\"\nspeed = \"ultrafast\"\nnote = \"Use for quick checks.\"\n";
    std::fs::write(h.paths.config(), format!("{config}{triage}")).unwrap();
    let (error, profiles) = chair.call("launch_profiles", json!({})).await;
    assert!(!error, "{profiles}");
    assert_eq!(
        serde_json::from_str::<Value>(&profiles).unwrap(),
        json!({
            "quick":{"runtime":"codex","speed":"ultrafast","note":"Use for quick checks."},
            "triage":{"runtime":"agy","model":"flash","effort":"high","note":"Use for test triage."}
        })
    );

    let mut args = json!({
        "action":"spawn", "profile":"triage", "committee":committee(), "cwd":project, "alias":"fast-review"
    });
    for (field, value) in [
        ("chair", json!(true)),
        ("secretary", json!(true)),
        ("committee", json!("other")),
    ] {
        let mut denied = args.clone();
        denied[field] = value;
        let (error, message) = chair.call("spawn", denied).await;
        assert!(
            error && message.contains("only the Speaker's office"),
            "{message}"
        );
    }
    args["attempt_id"] = json!(uuid::Uuid::now_v7().to_string());
    let (error, message) = chair.call("spawn", args.clone()).await;
    assert!(!error, "{message}");
    let seat: SpawnReply = serde_json::from_str(&message).unwrap();
    assert_eq!(seat.resolved_model.as_deref(), Some("gemini-3.8-flash"));
    assert_eq!(seat.display_name.as_deref(), Some("fast-review"));
    let (retry_error, retry_message) = chair.call("spawn", args).await;
    assert!(!retry_error, "{retry_message}");
    assert_eq!(
        serde_json::from_str::<SpawnReply>(&retry_message).unwrap(),
        seat
    );
    let terminal: PathBuf = ipc::request(
        &h.socket(),
        &Request::Attach {
            senator: seat.senator.into(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let argv = hosted_fixture_argv(&terminal, &project, WAIT).await;
    assert!(
        argv.contains(&"--model=gemini-3.8-flash".to_owned()),
        "{argv:?}"
    );
    assert!(argv.contains(&"--effort=high".to_owned()), "{argv:?}");

    // A profile that does not parse is reported by name, not passed over
    // in favour of what the daemon read earlier.
    let broken = "[profiles.broken]\nruntime = \"nope\"\nnote = \"x\"\n";
    std::fs::write(h.paths.config(), format!("{config}{triage}{broken}")).unwrap();
    let (error, message) = chair.call("launch_profiles", json!({})).await;
    assert!(error, "{message}");
    assert!(message.contains("[profiles.broken]"), "{message}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_tool_enforces_authority_and_records_the_actor() {
    use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
    use senate_bridge::ipc::{HookReply, HostedRuntime, SpawnReply, StopReply};
    use std::os::unix::fs::PermissionsExt as _;

    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let program = h.dir.path().join("fixture-tui");
    std::fs::write(
        &program,
        "#!/bin/sh\nwhile [ -d \"$PWD\" ]; do sleep 0.1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            program.to_str().unwrap()
        ),
    )
    .unwrap();
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            env!("CARGO_BIN_EXE_senate").into(),
        )
        .await
        .unwrap(),
    );
    let project = h.workdir("tui", None);
    let spawn = |alias, committee| Request::Spawn {
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee,
        alias: Some(path(alias)),
        cwd: project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    let socket = h.socket();
    let launched = |request: Request| {
        let socket = socket.clone();
        async move {
            ipc::request::<SpawnReply>(&socket, &request, WAIT, WAIT)
                .await
                .unwrap()
        }
    };
    let chair = launched(spawn("hosted-chair", committee())).await;
    let member = launched(spawn("hosted-member", committee())).await;
    let other = launched(spawn("other-host", "other".parse().unwrap())).await;
    let manual = |id: &str| {
        Request::SessionHook(
            serde_json::from_value(json!({
                "event":"session_start", "runtime":"claude", "session_id":id,
                "cwd":project, "committee_file":"refund-v2", "source":"startup",
            }))
            .unwrap(),
        )
    };
    for id in ["office-fixture", "manual-fixture"] {
        let _: HookReply = ipc::request(&h.socket(), &manual(id), WAIT, WAIT)
            .await
            .unwrap();
    }
    let office = path("claude-office-fixture");
    {
        let mut server = h.server.state.inner.lock().unwrap();
        server.chair = Some((chair.senator.clone(), true));
        server
            .roster
            .name_speaker(Some(path("alice").into()))
            .unwrap();
        server
            .roster
            .set_secretaries(path("alice"), vec![office.clone()])
            .unwrap();
    }

    let mut from_member = McpClient::start(h.socket(), selector(&member.session_id)).await;
    let mut from_chair = McpClient::start(h.socket(), selector(&chair.session_id)).await;
    let mut from_office = McpClient::start(h.socket(), selector("office-fixture")).await;
    let stop = |name| json!({"action":"stop", "senator":name});
    let (error, message) = from_member.call("spawn", stop("hosted-chair")).await;
    assert!(
        error && message.contains("only the Speaker's office or this committee's chair"),
        "{message}"
    );
    for (target, reason) in [
        (
            "hosted-chair",
            "Ask your operator to run senate session stop",
        ),
        ("other-host", "another committee"),
        ("claude-manual-fixture", "has no Senate-owned terminal"),
    ] {
        let (error, message) = from_chair.call("spawn", stop(target)).await;
        assert!(error && message.contains(reason), "{message}");
    }
    let forced = from_chair
        .rpc(
            "tools/call",
            json!({"name":"spawn", "arguments":{
                "action":"stop", "senator":"hosted-member", "force":true,
            }}),
        )
        .await;
    assert!(
        forced.get("error").is_some() || forced["result"]["isError"] == true,
        "{forced}"
    );
    assert!(
        h.store()
            .session(&member.session_id)
            .unwrap()
            .unwrap()
            .is_live()
    );

    let terminal: PathBuf = ipc::request(
        &h.socket(),
        &Request::Attach {
            senator: member.senator.clone().into(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let mut viewer = senate_bridge::host::open_fixture_viewer(&terminal, true)
        .await
        .unwrap();
    use tokio::io::AsyncReadExt as _;
    assert_eq!(viewer.read_u8().await.unwrap(), 3);
    viewer.write_all(b"\x01\0\x18\0\x50").await.unwrap();
    viewer.write_all(b"\x02\x00\x01x").await.unwrap();
    let viewer_record = || -> Option<Value> {
        serde_json::from_slice(&std::fs::read(terminal.with_file_name("viewer.json")).ok()?).ok()
    };
    eventually("the viewer to attend the hosted member", WAIT, || {
        (viewer_record()?["input_at"].as_u64()? > 0).then_some(())
    })
    .await;
    let (error, message) = from_chair.call("spawn", stop("hosted-member")).await;
    assert!(
        error && message.contains("a viewer is attending"),
        "{message}"
    );
    drop(viewer);
    eventually("the viewer to detach from the hosted member", WAIT, || {
        (viewer_record()?["attached"] == false).then_some(())
    })
    .await;

    h.server.state.inner.lock().unwrap().unseated = Some(member.senator.clone());
    let (error, message) = from_chair.call("spawn", stop("hosted-member")).await;
    assert!(
        error && message.contains("no longer a member of your committee"),
        "{message}"
    );
    assert!(terminal.exists());
    h.server.state.inner.lock().unwrap().unseated = None;

    for (client, target, launch, sender) in [
        (
            &mut from_chair,
            "hosted-member",
            &member,
            Address::senator(&chair.senator),
        ),
        (&mut from_office, "other-host", &other, Address::Speaker),
    ] {
        let (error, message) = client.call("spawn", stop(target)).await;
        assert!(!error, "{message}");
        let stopped: StopReply = serde_json::from_str(&message).unwrap();
        assert_eq!(stopped.senator, launch.senator);
        assert_eq!(stopped.session_id, launch.session_id);
        assert!(!stopped.audit_pending);
        assert_eq!(
            h.store()
                .session(&launch.session_id)
                .unwrap()
                .unwrap()
                .state,
            senate_bridge::SessionState::Ended
        );
        let notes = h.server.events(launch.committee.as_str());
        let note = notes
            .iter()
            .find(|event| event.body.contains("stopped the hosted terminal"))
            .unwrap();
        assert_eq!(note.from, sender);
        assert!(note.body.contains(launch.senator.as_str()));
        if sender == Address::Speaker {
            assert_eq!(note.meta.get(meta::ACTED_BY), Some(&json!(&office)));
        } else {
            assert!(note.body.contains(chair.senator.as_str()));
        }
    }
    let _: StopReply = ipc::request(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: chair.senator.into(),
            force: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_answer_and_catch_up_end_to_end() {
    let mut h = Harness::new().await;
    // This scenario binds by the branch name, which is off unless asked for.
    h.config.committee_from_branch = true;
    h.start().await;
    // bob's agent has connected before: a senator of this Senate.
    h.server.seat("copilot", "bob");
    let repo = h.dir.path().join("work/repo");
    git_repo(&repo, "senate/refund-v2/alice");
    h.hook(HookKind::SessionStart, "m1", &repo, None)
        .await
        .unwrap();
    let s = h.store().session("m1").unwrap().unwrap();
    assert_eq!(s.branch.as_deref(), Some("senate/refund-v2/alice"));
    assert_eq!(s.committee(), Some(&committee()));
    assert_eq!(s.effective_source(), Some(CommitteeSource::Branch));
    h.wait_stream("refund-v2").await;
    let mut mcp = McpClient::start(h.socket(), selector("m1")).await;

    // The tool list.
    let tools = mcp.request("tools/list", json!({})).await;
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "answer",
            "ask",
            "card",
            "catch_up",
            "checkpoint",
            "duty",
            "escalate",
            "inbox",
            "join",
            "launch_profiles",
            "memory_search",
            "post",
            "propose_decision",
            "receipt",
            "search",
            "secretary",
            "secretary_cover",
            "secretary_work",
            "status",
            "take",
            "terminal",
            "thread"
        ]
    );
    let tool = |name: &str| {
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()
            .clone()
    };
    for (name, params) in [
        ("ask", &["to", "body", "refs"][..]),
        ("search", &["query", "limit", "committee"]),
        ("thread", &["seq", "only", "committee"]),
        ("post", &["body", "committee"]),
        ("propose_decision", &["body", "committee"]),
    ] {
        let t = tool(name);
        for p in params {
            assert!(
                t["inputSchema"]["properties"].get(p).is_some(),
                "{name}.{p}"
            );
        }
    }

    let escalate = tool("escalate");
    assert!(
        escalate["description"]
            .as_str()
            .unwrap()
            .contains("Omit `to`")
    );
    assert!(
        escalate["inputSchema"]["properties"]["to"]["description"]
            .as_str()
            .unwrap()
            .contains("one step up")
    );

    // ask: from me, to bob, refs with my branch filled in.
    let (err, text) = mcp
        .call(
            "ask",
            json!({"to": "senator::copilot", "body": "网关的幂等键用哪个 header？", "refs": {"path": "gateway/src/idem.rs"}}),
        )
        .await;
    assert!(!err, "{text}");
    assert!(
        text.contains("Asked [senator::copilot] on refund-v2 (question seq 1)"),
        "{text}"
    );
    let q = h.server.last_event("refund-v2");
    assert_eq!(q.kind, Kind::Question);
    assert_eq!(q.from, claude("alice"));
    assert_eq!(q.to, vec![claude("bob")]);
    assert_eq!(q.thread, None);
    let refs = q.refs.unwrap();
    assert_eq!(refs.path.as_deref(), Some("gateway/src/idem.rs"));
    assert_eq!(refs.branch.as_deref(), Some("senate/refund-v2/alice"));

    // answer a relayed question: to the person the Speaker spoke for.
    let relay = h.server.append(relayed_question("退款上限是多少天？"));
    assert_eq!(relay.seq, Seq(2));
    let (err, text) = mcp
        .call("answer", json!({"thread": 2, "body": "30 天"}))
        .await;
    assert!(!err, "{text}");
    assert!(text.contains("addressed to [senator::pm]"), "{text}");
    let a = h.server.last_event("refund-v2");
    assert_eq!(
        (a.kind.clone(), a.from.clone()),
        (Kind::Answer, claude("alice"))
    );
    assert_eq!(a.to, vec![person("pm")]);
    assert_eq!(a.thread, Some(Seq(2)));

    // answer bob's question: to bob.
    let bq = h
        .server
        .append(question(claude("bob"), claude("alice"), "which header?"));
    assert_eq!(bq.seq, Seq(4));
    let (err, _) = mcp
        .call("answer", json!({"thread": 4, "body": "X-Idempotency-Key"}))
        .await;
    assert!(!err);
    let a = h.server.last_event("refund-v2");
    assert_eq!(
        (a.to.clone(), a.thread),
        (vec![claude("bob")], Some(Seq(4)))
    );

    // Given a seq inside a thread, the answer goes to the thread's root.
    let corr = h.server.append(
        NewEvent::new(
            committee(),
            Kind::Chat,
            person("pm"),
            "不对，用 Idempotency-Key",
        )
        .with_to(vec![claude("alice"), claude("bob")])
        .in_thread(Seq(4)),
    );
    assert_eq!(corr.seq, Seq(6));
    let (err, _) = mcp.call("answer", json!({"thread": 6, "body": "ok"})).await;
    assert!(!err);
    let a = h.server.last_event("refund-v2");
    assert_eq!(
        (a.to.clone(), a.thread, a.seq),
        (vec![claude("bob")], Some(Seq(4)), Seq(7))
    );

    let (err, text) = mcp
        .call("answer", json!({"thread": 999, "body": "?"}))
        .await;
    assert!(err);
    assert!(text.contains("no event with seq 999"), "{text}");

    // catch_up: directed first in full, decisions in full, the rest cut.
    let decision = "D".repeat(600);
    let long = "N".repeat(600);
    h.server.append(NewEvent::new(
        committee(),
        Kind::Decision,
        person("pm"),
        decision.clone(),
    ));
    h.server.append(note(person("pm"), &long));
    // The directed ones (2, 4, 6) reached the daemon and wait for this session's hooks.
    eventually("directed items held", WAIT, || {
        [2u64, 4, 6]
            .iter()
            .all(|s| h.item(*s).is_some_and(|i| i.status == ItemStatus::Queued))
            .then_some(())
    })
    .await;
    assert_eq!(tool("receipt")["annotations"]["readOnlyHint"], true);
    let marks = h.store().seen(&committee()).unwrap();
    let local = h.store().counts().unwrap();
    let event_count = h.server.events("refund-v2").len();
    let last_seen = h.store().session("m1").unwrap().unwrap().last_seen;
    let read = h.server.cursor("refund-v2", "claude", "read");
    let (err, receipt) = mcp
        .call("receipt", json!({"committee":"refund-v2","seq":2}))
        .await;
    assert!(
        !err && receipt.contains("recorded") && receipt.contains("queued"),
        "{receipt}"
    );
    assert_eq!(h.store().seen(&committee()).unwrap(), marks);
    assert_eq!(h.store().counts().unwrap(), local);
    assert_eq!(h.server.events("refund-v2").len(), event_count);
    assert_eq!(
        h.store().session("m1").unwrap().unwrap().last_seen,
        last_seen
    );
    assert_eq!(h.server.cursor("refund-v2", "claude", "read"), read);
    let (err, missing) = mcp
        .call("receipt", json!({"committee":"refund-v2","seq":9999}))
        .await;
    assert!(err && missing.contains("No event recorded"), "{missing}");
    let (err, text) = mcp.call("catch_up", json!({})).await;
    assert!(!err, "{text}");
    assert!(
        text.contains("9 event(s), seq 1–9, 3 addressed to you ([senator::claude])"),
        "{text}"
    );
    let order = [
        at(&text, "── Addressed to you ──"),
        at(&text, "· seq 2 ·"),
        at(&text, "· seq 4 ·"),
        at(&text, "· seq 6 ·"),
        at(&text, "── Everything else"),
        at(&text, "· seq 1 ·"),
        at(&text, "· seq 3 ·"),
        at(&text, "· seq 5 ·"),
        at(&text, "· seq 7 ·"),
        at(&text, "· seq 8 ·"),
        at(&text, "· seq 9 ·"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    assert!(text.contains(&decision), "decisions stay whole");
    assert!(text.contains(&format!("{}…", "N".repeat(300))));
    assert!(!text.contains(&"N".repeat(301)));
    assert_eq!(h.server.cursor("refund-v2", "claude", "read"), Some(Seq(9)));
    assert_eq!(h.server.cursor("refund-v2", "alice", "read"), None);
    for s in [2u64, 4, 6] {
        let item = h.item(s).unwrap();
        assert_eq!(
            item.status,
            ItemStatus::Delivered,
            "seq {s} read via catch_up"
        );
    }
    assert!(
        h.hook(HookKind::UserPromptSubmit, "m1", &repo, None)
            .await
            .is_none(),
        "nothing left to hand over after catch_up"
    );

    let (err, delivered) = mcp
        .call("receipt", json!({"committee":"refund-v2","seq":2}))
        .await;
    // The agent read it itself: seen, not merely handed over.
    assert!(
        !err && delivered.contains("[senator::claude]  seen (catch_up)"),
        "{delivered}"
    );
    let (_, text) = mcp.call("catch_up", json!({})).await;
    assert!(
        text.contains("Nothing new on refund-v2 since seq 9"),
        "{text}"
    );

    for i in 10..=12 {
        h.server.append(note(person("pm"), &format!("note {i}")));
    }
    let (_, text) = mcp.call("catch_up", json!({"limit": 2})).await;
    assert!(text.contains("More remain"), "{text}");
    assert_eq!(
        h.server.cursor("refund-v2", "claude", "read"),
        Some(Seq(11))
    );
    let (_, text) = mcp.call("catch_up", json!({})).await;
    assert!(text.contains("1 event(s), seq 12–12"), "{text}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn join_binds_a_session_and_failures_explain_themselves() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("u", None);
    assert!(
        h.hook(HookKind::SessionStart, "u1", &wd, None)
            .await
            .is_none()
    );
    let mut mcp = McpClient::start(h.socket(), selector("u1")).await;

    let (err, text) = mcp.call("ask", json!({"to": "bob", "body": "hi"})).await;
    assert!(err);
    assert!(text.contains("not joined to a Senate committee"), "{text}");
    let (err, text) = mcp.call("join", json!({"committee": "nope"})).await;
    assert!(err);
    assert!(text.contains("cannot join `nope`"), "{text}");
    let (err, _) = mcp.call("join", json!({"committee": "Not A Slug"})).await;
    assert!(err);

    let (err, text) = mcp.call("join", json!({"committee": "refund-v2"})).await;
    assert!(!err, "{text}");
    assert!(text.contains("now joined to refund-v2"), "{text}");
    let s = h.store().session("u1").unwrap().unwrap();
    assert_eq!(s.effective_source(), Some(CommitteeSource::Join));
    h.wait_stream("refund-v2").await;
    // A later hook re-infers nothing and keeps the join.
    h.hook(HookKind::UserPromptSubmit, "u1", &wd, None).await;
    assert_eq!(
        h.store().session("u1").unwrap().unwrap().committee(),
        Some(&committee())
    );

    // A bare name is that senator: `copilot` is the agent bob runs.
    h.server.seat("copilot", "bob");
    let (err, text) = mcp
        .call("ask", json!({"to": "copilot", "body": "hi"}))
        .await;
    assert!(!err, "{text}");
    assert_eq!(h.server.last_event("refund-v2").to, vec![claude("bob")]);

    let mut ghost = McpClient::start(h.socket(), selector("ghost")).await;
    let (err, text) = ghost.call("catch_up", json!({})).await;
    assert!(err);
    assert!(
        text.contains("not registered with the Senate bridge"),
        "{text}"
    );

    // After /clear: the old id has ended, a new session runs in the same
    // Claude process. The MCP server still has the old id; the pid finds it.
    h.hook(HookKind::SessionEnd, "u1", &wd, None).await;
    let wd2 = h.workdir("u2", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "u2", &wd2, None).await;
    let mut after_clear = McpClient::start(
        h.socket(),
        SessionSelector {
            session_id: Some("u1".into()),
            claude_pid: Some(std::process::id()),
        },
    )
    .await;
    let (err, text) = after_clear
        .call("ask", json!({"to": "senator::copilot", "body": "status?"}))
        .await;
    assert!(!err, "{text}");
    let last = h.server.last_event("refund-v2");
    assert_eq!(last.to, vec![claude("bob")]);
    assert_eq!(last.refs, None, "u2 is not in a git repo, so no branch");
    h.stop().await;
}

/// `ask` reaches agents only, in the tool and in the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_refuses_recipients_that_would_never_answer() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("a", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "a1", &wd, None).await;
    let before = h.server.events("refund-v2").len();
    let mut mcp = McpClient::start(h.socket(), selector("a1")).await;
    for to in ["speaker", "senator::pm", "senator::copilot, speaker"] {
        let (err, text) = mcp.call("ask", json!({"to": to, "body": "?"})).await;
        assert!(err, "{to}");
        assert!(text.contains("only reach a teammate's agent"), "{text}");
    }
    // Legacy single-colon forms are not addresses.
    for to in ["agent:bob", "user:pm"] {
        let (err, text) = mcp.call("ask", json!({"to": to, "body": "?"})).await;
        assert!(err && text.contains("is not an address"), "{to}: {text}");
    }
    let tools = mcp.request("tools/list", json!({})).await;
    let ask = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "ask")
        .unwrap();
    let description = ask["description"].as_str().unwrap();
    assert!(
        description.contains("A person is not reached through `ask`")
            && description.contains("senator::*")
            && description.contains("ledger::codex"),
        "{description}"
    );

    // A client that skips the tool's check meets the daemon's.
    for to in [
        vec![Address::Speaker],
        vec![person("pm")],
        vec![claude("alice")],
    ] {
        let err = ipc::request::<Posted>(
            &h.socket(),
            &Request::Ask {
                session: selector("a1"),
                to: to.clone(),
                body: "?".into(),
                refs: None,
                id: None,
                request: false,
            },
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ClientError::Remote(ref e) if e.code == "bad_request"),
            "{to:?}: {err}"
        );
    }
    assert_eq!(h.server.events("refund-v2").len(), before, "nothing posted");
    h.stop().await;
}

/// A resent ask or answer carrying the same id is posted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resent_request_with_the_same_id_posts_once() {
    let mut h = Harness::new().await;
    h.start().await;
    // bob's agent has connected before: a senator of this Senate.
    h.server.seat("copilot", "bob");
    let wd = h.workdir("a", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "a1", &wd, None).await;
    let id = EventId::new();
    let ask = Request::Ask {
        session: selector("a1"),
        to: vec![claude("bob")],
        body: "which header?".into(),
        refs: None,
        id: Some(id.clone()),
        request: false,
    };
    let call = |r: Request| {
        let socket = h.socket();
        async move {
            ipc::request::<Posted>(&socket, &r, Duration::from_secs(1), Duration::from_secs(5))
                .await
                .unwrap()
        }
    };
    let first = call(ask.clone()).await;
    let again = call(ask).await;
    assert!(!first.duplicate);
    assert!(again.duplicate);
    assert_eq!(first.seq, again.seq);
    assert_eq!(h.server.events("refund-v2").len(), 1);
    assert_eq!(h.server.last_event("refund-v2").id, id);

    let answer_id = EventId::new();
    let answer = Request::Answer {
        session: selector("a1"),
        thread: first.seq,
        body: "self-note".into(),
        id: Some(answer_id.clone()),
        committee: None,
        agent: None,
        as_person: false,
    };
    let a1 = call(answer.clone()).await;
    let a2 = call(answer).await;
    assert_eq!((a1.seq, a2.duplicate), (a2.seq, true));
    assert_eq!(h.server.events("refund-v2").len(), 2);
    h.stop().await;
}

/// Without `CLAUDE_PID` the hook reports the pids above it, and the MCP
/// shim (whose parent is Claude) finds its session through them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_is_found_by_a_pid_above_its_hook() {
    let mut h = Harness::new().await;
    h.start().await;
    // bob's agent has connected before: a senator of this Senate.
    h.server.seat("copilot", "bob");
    let wd = h.workdir("p", Some("refund-v2"));
    let env = HookEnv {
        socket: None,
        token: None,
        claude_pid: None,
        session_id: None,
        agent: None,
    };
    h.hook_with(HookKind::SessionStart, "p1", &wd, &env)
        .await
        .unwrap();
    assert_eq!(h.store().session("p1").unwrap().unwrap().claude_pid, None);
    // The hook ran in this process, so this process's parent is above it,
    // as Claude is above a real hook.
    let above = std::os::unix::process::parent_id();
    assert_eq!(h.store().sessions_below(above).unwrap(), ["p1"]);
    let mut mcp = McpClient::start(
        h.socket(),
        SessionSelector {
            session_id: None,
            claude_pid: Some(above),
        },
    )
    .await;
    let (err, text) = mcp
        .call("ask", json!({"to": "copilot", "body": "found me?"}))
        .await;
    assert!(!err, "{text}");
    assert_eq!(h.server.last_event("refund-v2").body, "found me?");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_post_and_propose_decision_end_to_end() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("s1")).await;

    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Chat,
            person("pm"),
            "后端群：幂等键用哪个 header？",
        )
        .with_source(chat_source()),
    );
    h.server
        .append(note(claude("bob"), &format!("幂等键 {}", "长".repeat(400))));
    h.server.append(NewEvent::new(
        committee(),
        Kind::Decision,
        person("pm"),
        format!("幂等键统一用 Idempotency-Key {}", "全".repeat(400)),
    ));
    h.server.append(note(claude("bob"), "unrelated"));

    let (err, text) = mcp
        .call("search", json!({"query": "幂等键", "limit": 5}))
        .await;
    assert!(!err, "{text}");
    assert!(
        text.starts_with("[senate] Search on refund-v2 for \"幂等键\": 3 result(s)"),
        "{text}"
    );
    assert!(text.contains("not from your user"));
    // Best (here: newest) first, each with seq, sender, kind and time.
    let decision = at(&text, "· seq 3 · from [senator::pm] · decision · ");
    let note_hit = at(&text, "· seq 2 · from [senator::copilot] · note · ");
    // A hit said in a chat group names the group it was said in.
    let chat = at(
        &text,
        "· seq 1 · from [senator::pm] in chat group「退款 v2 · 后端群」 · chat · ",
    );
    assert!(decision < note_hit && note_hit < chat);
    assert!(text.contains(" UTC]\n│ "));
    assert!(text.contains(&"全".repeat(400)), "decisions in full");
    assert!(!text.contains(&"长".repeat(301)), "other bodies cut");
    assert!(!text.contains("unrelated"));
    assert_eq!(
        h.server.searches().last().unwrap(),
        &("refund-v2".to_string(), "幂等键".to_string(), Some(5))
    );

    let (err, text) = mcp
        .call("search", json!({"query": "nothing-like-this"}))
        .await;
    assert!(!err, "{text}");
    assert!(text.contains("Nothing on refund-v2 matches"), "{text}");
    let (err, text) = mcp.call("search", json!({"query": "  "})).await;
    assert!(err);
    assert!(text.contains("`query` is empty"), "{text}");
    mcp.call("search", json!({"query": "x", "committee": "other"}))
        .await;
    assert_eq!(h.server.searches().last().unwrap().0, "other");

    // What search cut is read whole with `thread`.
    let (err, text) = mcp.call("thread", json!({"seq": 2})).await;
    assert!(!err, "{text}");
    assert!(
        text.starts_with("[senate] Thread 2 on refund-v2: 1 event(s), oldest first."),
        "{text}"
    );
    assert!(text.contains("· seq 2 · from [senator::copilot] · note · "));
    assert!(text.contains(&"长".repeat(400)), "{text}");
    let (err, text) = mcp.call("thread", json!({"seq": 99})).await;
    assert!(err, "{text}");

    let (err, text) = mcp
        .call("post", json!({"body": "网关已切到新的幂等中间件"}))
        .await;
    assert!(!err, "{text}");
    assert!(
        text.contains("Posted a note on refund-v2 (seq 5)"),
        "{text}"
    );
    let posted = h.server.last_event("refund-v2");
    assert_eq!(posted.kind, Kind::Note);
    assert_eq!(posted.from, claude("alice"));
    assert!(posted.to.is_empty());
    assert!(!posted.meta.contains_key(meta::PROPOSAL));

    let (err, text) = mcp
        .call(
            "propose_decision",
            json!({"body": "幂等键统一用 Idempotency-Key"}),
        )
        .await;
    assert!(!err, "{text}");
    assert!(
        text.contains("Proposed a decision on refund-v2 (seq 6)"),
        "{text}"
    );
    assert!(text.contains("not a decision yet"), "{text}");
    let proposed = h.server.last_event("refund-v2");
    assert_eq!(proposed.kind, Kind::Note);
    assert_eq!(proposed.from, claude("alice"));
    assert!(proposed.to.is_empty());
    assert_eq!(proposed.meta[meta::PROPOSAL], json!(PROPOSAL_DECISION));
    let (_, text) = mcp
        .call("search", json!({"query": "Idempotency-Key 统一"}))
        .await;
    assert!(text.contains("· note (decision proposal) · "), "{text}");

    let (err, text) = mcp.call("post", json!({"body": " "})).await;
    assert!(err);
    assert!(text.contains("`body` is empty"), "{text}");

    // A seat's duty: its own by default, another seat's by name.
    let (err, text) = mcp
        .call("duty", json!({"duty": "  reviews\n the gateway "}))
        .await;
    assert!(!err, "{text}");
    assert!(text.contains("Set the duty on refund-v2"), "{text}");
    let set = h.server.last_event("refund-v2");
    assert_eq!(set.kind, Kind::Note);
    assert_eq!(set.from, claude("alice"));
    assert_eq!(
        set.meta[meta::DUTY],
        json!({"senator": "claude", "duty": "reviews the gateway"})
    );
    let (err, text) = mcp
        .call(
            "duty",
            json!({"duty": "ships", "senator": "senator::codex"}),
        )
        .await;
    assert!(!err, "{text}");
    assert_eq!(
        h.server.last_event("refund-v2").meta[meta::DUTY]["senator"],
        json!("codex")
    );
    let (err, text) = mcp.call("duty", json!({"duty": ""})).await;
    assert!(!err && text.contains("Cleared the duty"), "{text}");
    let (err, text) = mcp.call("duty", json!({"duty": "x".repeat(301)})).await;
    assert!(err && text.contains("at most 300"), "{text}");

    // A named, unregistered caller must resolve its identity even when it supplies a committee.
    let mut ghost = McpClient::start(h.socket(), selector("ghost")).await;
    let (err, text) = ghost.call("post", json!({"body": "hi"})).await;
    assert!(err);
    assert!(text.contains("not registered"), "{text}");
    let (err, text) = ghost
        .call("post", json!({"body": "hi", "committee": "other"}))
        .await;
    assert!(err && text.contains("not registered"), "{text}");
    let (err, _) = ghost
        .call("post", json!({"body": "hi", "committee": "Not A Slug"}))
        .await;
    assert!(err);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_brief_is_a_resource() {
    let mut h = Harness::new().await;
    h.start().await;
    h.server.append(NewEvent::new(
        committee(),
        Kind::Decision,
        person("pm"),
        "退款上限 30 天",
    ));
    let wd = h.workdir("r", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "r1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("r1")).await;

    let listed = mcp.request("resources/list", json!({})).await;
    let resources = listed["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1, "{listed}");
    assert_eq!(resources[0]["uri"], "senate://refund-v2/brief");
    assert_eq!(resources[0]["mimeType"], "text/markdown");
    let templates = mcp.request("resources/templates/list", json!({})).await;
    assert_eq!(
        templates["resourceTemplates"][0]["uriTemplate"],
        "senate://{committee}/brief"
    );

    let read = mcp
        .request("resources/read", json!({"uri": "senate://refund-v2/brief"}))
        .await;
    let contents = &read["contents"][0];
    assert_eq!(contents["uri"], "senate://refund-v2/brief");
    assert_eq!(contents["mimeType"], "text/markdown");
    let text = contents["text"].as_str().unwrap();
    assert!(
        text.starts_with("# Brief: refund-v2 (through seq 1)"),
        "{text}"
    );
    assert!(text.contains("[senator::pm]: 退款上限 30 天"), "{text}");

    let other = mcp
        .request("resources/read", json!({"uri": "senate://other/brief"}))
        .await;
    let text = other["contents"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("# Brief: other (through seq 0)"), "{text}");
    let bad = mcp
        .rpc("resources/read", json!({"uri": "file:///etc/passwd"}))
        .await;
    assert!(bad.get("error").is_some(), "{bad}");
    h.server.set_brief_broken(true);
    let broken = mcp
        .rpc("resources/read", json!({"uri": "senate://refund-v2/brief"}))
        .await;
    let message = broken["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("brief broke"), "{broken}");

    // An unbound session lists nothing, but can still read by URI.
    let wd = h.workdir("n", None);
    h.hook(HookKind::SessionStart, "n1", &wd, None).await;
    let mut unbound = McpClient::start(h.socket(), selector("n1")).await;
    let listed = unbound.request("resources/list", json!({})).await;
    assert_eq!(listed["resources"], json!([]));
    h.server.set_brief_broken(false);
    let read = unbound
        .request("resources/read", json!({"uri": "senate://refund-v2/brief"}))
        .await;
    assert!(
        read["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("退款上限")
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_escalate_take_and_inbox_end_to_end() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("s1")).await;
    let q = h.server.append(question(
        claude("bob"),
        claude("alice"),
        "网关幂等用哪个 header？",
    ));
    let r = h.server.append(
        NewEvent::new(
            committee(),
            Kind::Request,
            claude("bob"),
            "把 header 改成 Idempotency-Key",
        )
        .to(claude("alice")),
    );

    let (err, text) = mcp
        .call(
            "status",
            json!({"thread": q.seq.0, "state": "processing", "title": "查幂等 header",
                   "steps": [{"id": "read", "label": "已读日志", "done": true},
                             {"id": "grep", "label": "查 gateway/src/idem.rs"}]}),
        )
        .await;
    assert!(!err, "{text}");
    let status = h.server.last_event("refund-v2");
    assert_eq!(status.kind, Kind::Status);
    assert_eq!(status.thread, Some(q.seq));
    let payload = StatusPayload::of(&status).unwrap();
    assert_eq!(
        payload.session.as_deref(),
        Some("s1"),
        "the calling session owns it"
    );
    let owner = h.store().owner(&committee(), q.seq).unwrap().unwrap();
    assert_eq!(owner.agent, path("claude"));
    assert_eq!(payload.steps.unwrap().len(), 2);

    // Waiting on a person is escalate's job, not status's.
    let (err, text) = mcp
        .call("status", json!({"thread": q.seq.0, "state": "suspended"}))
        .await;
    assert!(err && text.contains("escalate"), "{text}");

    let (err, text) = mcp
        .call(
            "escalate",
            json!({"thread": r.seq.0, "to": "alice", "reason": "改接口要 alice 定"}),
        )
        .await;
    assert!(!err, "{text}");
    let escalated = StatusPayload::of(&h.server.last_event("refund-v2")).unwrap();
    assert_eq!(escalated.state, StatusState::Suspended);
    assert_eq!(escalated.escalated_to, Some(name("alice")));
    assert_eq!(h.server.last_event("refund-v2").from, claude("alice"));
    let (err, text) = mcp
        .call(
            "escalate",
            json!({"thread": r.seq.0, "to": "senator::copilot"}),
        )
        .await;
    assert!(err && text.contains("is not a person"), "{text}");
    let (err, text) = mcp
        .call("escalate", json!({"thread": r.seq.0, "to": "senator::pm"}))
        .await;
    assert!(!err, "{text}");
    let to_pm = StatusPayload::of(&h.server.last_event("refund-v2")).unwrap();
    assert_eq!(to_pm.escalated_to, Some(name("pm")));

    let (err, text) = mcp.call("inbox", json!({})).await;
    assert!(!err, "{text}");
    assert!(
        text.contains("2 open task thread(s) on refund-v2 for [senator::claude], [senator::alice]"),
        "{text}"
    );
    assert!(text.contains("查幂等 header"), "{text}");
    assert!(text.contains("escalated to [senator::pm]"), "{text}");

    let (err, text) = mcp.call("take", json!({"thread": q.seq.0})).await;
    assert!(!err, "{text}");
    let taken = h.server.last_event("refund-v2");
    assert_eq!(taken.from, person("alice"), "taken in the person's name");
    assert_eq!(
        StatusPayload::of(&taken).unwrap().takeover,
        Some(name("alice"))
    );
    let (_, text) = mcp.call("inbox", json!({})).await;
    assert!(text.contains("taken over by [senator::alice]"), "{text}");

    // The same inbox, in Chinese.
    let mut zh = McpClient::start_in(h.socket(), selector("s1"), Locale::Zh).await;
    let (_, text) = zh.call("inbox", json!({})).await;
    assert!(
        text.starts_with(
            "refund-v2 上有 2 个等着 [senator::claude], [senator::alice] 的任务 thread"
        ),
        "{text}"
    );
    assert!(text.contains("[senator::alice] 人工处理中"), "{text}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answered_thread_leaves_the_inbox_until_someone_says_more() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("s1")).await;
    let mut zh = McpClient::start_in(h.socket(), selector("s1"), Locale::Zh).await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "which header?"));

    let (_, text) = mcp.call("inbox", json!({})).await;
    assert!(text.contains("thread "), "{text}");
    assert!(!text.contains("not shown"), "{text}");

    let (err, text) = mcp
        .call(
            "answer",
            json!({"thread": q.seq.0, "body": "Idempotency-Key"}),
        )
        .await;
    assert!(!err, "{text}");
    let (_, text) = mcp.call("inbox", json!({})).await;
    assert!(text.starts_with("Nothing open waits on"), "{text}");
    assert!(
        text.ends_with("1 answered thread(s) not shown; all=true lists them."),
        "{text}"
    );
    let (_, text) = zh.call("inbox", json!({})).await;
    assert!(
        text.ends_with("已答复的 1 个 thread 未显示；用 all=true 列出。"),
        "{text}"
    );
    let (_, text) = mcp.call("inbox", json!({"all": true})).await;
    assert!(
        text.contains(&format!("thread {} ", q.seq.0)) && text.contains("settled"),
        "{text}"
    );

    // A follow-up from bob puts it back.
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Note,
            claude("bob"),
            "and the retry header?",
        )
        .to(claude("alice"))
        .in_thread(q.seq),
    );
    let (_, text) = mcp.call("inbox", json!({})).await;
    assert!(
        text.contains("said more after the answer") && !text.contains("not shown"),
        "{text}"
    );
    h.stop().await;
}

/// A Codex thread's MCP server shares the app-server's environment with
/// every other thread; each tool call's `_meta.threadId` names the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_codex_tool_call_is_resolved_by_the_thread_in_its_meta() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("cx", Some("refund-v2"));
    // The app-server was started from inside a Claude Code session, so its
    // hooks inherit that session's variables. None of them may stick.
    let leaky = HookEnv {
        socket: Some(h.dir.path().join("claude-inbox.sock")),
        token: Some("claude-token".into()),
        claude_pid: Some(std::process::id()),
        session_id: Some("claude-session".into()),
        agent: Some("reviewer".into()),
    };
    for thread in ["thread-a", "thread-b"] {
        let input = json!({"session_id":thread,"cwd":wd,"source":"startup"}).to_string();
        senate_bridge::hook::run_as(
            HookKind::SessionStart,
            &h.socket(),
            &input,
            &leaky,
            Some(Runtime::Codex),
            Default::default(),
        )
        .await
        .unwrap();
        let s = h.store().session(thread).unwrap().unwrap();
        assert_eq!(s.runtime, Runtime::Codex);
        assert_eq!((s.socket, s.token, s.claude_pid), (None, None, None));
        // Nor does the Claude session's SENATE_AGENT: a Codex thread is codex.
        assert_eq!(s.agent, path(&format!("codex-{thread}")));
    }
    let above = std::os::unix::process::parent_id();
    assert!(h.store().sessions_below(above).unwrap().is_empty());

    // The shim's own selector points elsewhere; the call's _meta wins.
    let mut mcp = McpClient::start(
        h.socket(),
        SessionSelector {
            session_id: Some("claude-session".into()),
            claude_pid: Some(above),
        },
    )
    .await;
    let r = mcp
        .request(
            "tools/call",
            json!({
                "name": "join",
                "arguments": {"committee": "other"},
                "_meta": {
                    "callId": "call_1",
                    "threadId": "thread-b",
                    "x-codex-turn-metadata": {"thread_id": "thread-b", "turn_id": "t1"}
                }
            }),
        )
        .await;
    assert_ne!(r["isError"], json!(true), "{r}");
    let joined = |id: &str| {
        h.store()
            .session(id)
            .unwrap()
            .unwrap()
            .committee()
            .map(|f| f.to_string())
    };
    assert_eq!(joined("thread-b").as_deref(), Some("other"));
    assert_eq!(joined("thread-a").as_deref(), Some("refund-v2"));

    // Two Codex threads share a worktree. The tool can name just its caller.
    let r = mcp
        .request(
            "tools/call",
            json!({
                "name": "join", "arguments": {"committee": "refund-v2", "senator": "reviewer"},
                "_meta": {"threadId": "thread-b"}
            }),
        )
        .await;
    assert_ne!(r["isError"], json!(true), "{r}");
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[senator::reviewer]")
    );
    for (id, expected) in [
        ("thread-a", "codex-thread-a"),
        ("thread-b", "codex-thread-b"),
    ] {
        assert_eq!(
            h.store().session(id).unwrap().unwrap().agent,
            path(expected)
        );
    }
    h.server.seat("owned", "bob");
    for name in ["speaker", "not/a/name", "", "alice", "owned"] {
        let r = mcp
            .request(
                "tools/call",
                json!({
                    "name": "join", "arguments": {"committee": "other", "senator": name},
                    "_meta": {"threadId": "thread-b"}
                }),
            )
            .await;
        assert_eq!(r["isError"], json!(true), "{name}: {r}");
        assert_eq!(joined("thread-b").as_deref(), Some("refund-v2"));
        assert_eq!(
            h.store().session("thread-b").unwrap().unwrap().agent,
            path("codex-thread-b")
        );
    }

    // Only the turn metadata: still that thread. An unknown thread is not
    // registered, and is not mistaken for another session.
    let r = mcp
        .request(
            "tools/call",
            json!({
                "name": "catch_up",
                "arguments": {},
                "_meta": {"x-codex-turn-metadata": {"thread_id": "thread-a"}}
            }),
        )
        .await;
    assert_ne!(r["isError"], json!(true), "{r}");
    let r = mcp
        .request(
            "tools/call",
            json!({"name": "catch_up", "arguments": {}, "_meta": {"threadId": "thread-zz"}}),
        )
        .await;
    assert_eq!(r["isError"], json!(true), "{r}");
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not registered"),
        "{r}"
    );
    h.stop().await;
}

/// `ask` to `senator::*` when several agents are on the committee (here
/// the two bob runs and the asker's own): the server refuses with the
/// candidates, the tool result lists them, and the agent names one and asks
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_recipient_comes_back_as_a_choice_and_the_retry_lands() {
    let mut h = Harness::new().await;
    h.start().await;
    h.server.seat("copilot", "bob");
    h.server.seat("aider", "bob");
    let wd = h.workdir("a", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "a1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("a1")).await;
    let before = h.server.events("refund-v2").len();

    for to in ["*", "senator::*", "[senator::*]"] {
        let (err, text) = mcp
            .call("ask", json!({"to": to, "body": "which header?"}))
            .await;
        assert!(err, "{to}: {text}");
        assert!(
            text.starts_with(
                "[senator::*] could reach several agents on this committee: \
                 [senator::copilot], [senator::aider], [senator::claude]."
            ),
            "{text}"
        );
        assert!(text.contains("Name one of them in `to` and ask again."));
        assert!(
            text.ends_with("\n- senator::copilot\n- senator::aider\n- senator::claude"),
            "{text}"
        );
    }
    assert_eq!(h.server.events("refund-v2").len(), before, "nothing stored");

    // The daemon's reply carries the candidates as data, too.
    let err = ipc::request::<Posted>(
        &h.socket(),
        &Request::Ask {
            session: selector("a1"),
            to: vec!["senator::*".parse().unwrap()],
            body: "?".into(),
            refs: None,
            id: None,
            request: false,
        },
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    let ClientError::Remote(e) = err else {
        panic!("{err}")
    };
    assert_eq!(e.code, "ambiguous_recipient");
    assert_eq!(
        e.candidates,
        [path("copilot"), path("aider"), path("claude")]
    );

    // Named: it lands, stored with the concrete agent.
    let (err, text) = mcp
        .call(
            "ask",
            json!({"to": "senator::aider", "body": "which header?"}),
        )
        .await;
    assert!(!err, "{text}");
    assert!(text.contains("Asked [senator::aider]"), "{text}");
    assert_eq!(h.server.last_event("refund-v2").to, vec![senator("aider")]);

    // In Chinese, the same choice.
    let mut zh = McpClient::start_in(h.socket(), selector("a1"), Locale::Zh).await;
    let (err, text) = zh.call("ask", json!({"to": "*", "body": "?"})).await;
    assert!(err);
    assert!(
        text.starts_with(
            "[senator::*] 在这个委员会有好几个 agent 可以接：[senator::copilot], [senator::aider], [senator::claude]"
        ),
        "{text}"
    );
    h.stop().await;
}

/// Two agents of one person on one machine each read their own inbox and
/// catch up on their own read cursor; what one was asked is not the
/// other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_agent_has_its_own_inbox_and_read_position() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("w", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "cc", &wd, None).await;
    let mut env = h.env_for("rv", None);
    env.agent = Some("reviewer".into());
    h.hook_with(HookKind::SessionStart, "rv", &wd, &env).await;
    let mut claude_mcp = McpClient::start(h.socket(), selector("cc")).await;
    let mut reviewer_mcp = McpClient::start(h.socket(), selector("rv")).await;

    h.server
        .append(question(claude("bob"), claude("alice"), "for claude"));
    h.server.append(question(
        claude("bob"),
        senator("reviewer"),
        "for the reviewer",
    ));

    let (_, text) = claude_mcp.call("inbox", json!({})).await;
    assert!(
        text.contains("for claude") && !text.contains("for the reviewer"),
        "{text}"
    );
    let (_, text) = reviewer_mcp.call("inbox", json!({})).await;
    assert!(
        text.contains("for the reviewer") && !text.contains("for claude"),
        "{text}"
    );
    assert!(text.contains("[senator::reviewer]"), "{text}");

    let (_, text) = claude_mcp.call("catch_up", json!({})).await;
    assert!(
        text.contains("1 addressed to you ([senator::claude])"),
        "{text}"
    );
    let (_, text) = reviewer_mcp.call("catch_up", json!({})).await;
    // The reviewer's read position is its own: it still reads both events.
    assert!(
        text.contains("2 event(s), seq 1–2, 1 addressed to you ([senator::reviewer])"),
        "{text}"
    );
    assert_eq!(h.server.cursor("refund-v2", "claude", "read"), Some(Seq(2)));
    assert_eq!(
        h.server.cursor("refund-v2", "reviewer", "read"),
        Some(Seq(2))
    );

    // Posting as each agent writes from that agent.
    claude_mcp
        .call("post", json!({"body": "from claude"}))
        .await;
    assert_eq!(h.server.last_event("refund-v2").from, claude("alice"));
    reviewer_mcp
        .call("post", json!({"body": "from reviewer"}))
        .await;
    assert_eq!(h.server.last_event("refund-v2").from, senator("reviewer"));

    // One note tells several members at once: a chair's rule for everyone
    // needs one call, not one per member.
    let (err, text) = claude_mcp
        .call(
            "post",
            json!({"to": "reviewer, senator::tester", "body": "build in your own worktree"}),
        )
        .await;
    assert!(!err, "{text}");
    assert!(
        text.starts_with("Told [senator::reviewer], [senator::tester] on refund-v2"),
        "{text}"
    );
    assert_eq!(
        h.server.last_event("refund-v2").to,
        [senator("reviewer"), senator("tester")]
    );
    let (_, text) = reviewer_mcp.call("catch_up", json!({})).await;
    assert!(
        text.contains("build in your own worktree") && text.contains("1 addressed to you"),
        "{text}"
    );
    h.stop().await;
}

/// Tool results are written in the shim's locale; tool names and fields are
/// not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_results_follow_the_locale() {
    let mut h = Harness::new().await;
    h.start().await;
    // bob's agent has connected before: a senator of this Senate.
    h.server.seat("copilot", "bob");
    let wd = h.workdir("z", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "z1", &wd, None).await;
    let mut mcp = McpClient::start_in(h.socket(), selector("z1"), Locale::Zh).await;
    let (err, text) = mcp
        .call(
            "ask",
            json!({"to": "senator::copilot", "body": "用哪个 header？"}),
        )
        .await;
    assert!(!err, "{text}");
    assert!(
        text.starts_with("已在 refund-v2 上问了 [senator::copilot]（问题 seq 1）"),
        "{text}"
    );
    let (_, text) = mcp.call("catch_up", json!({})).await;
    assert!(
        text.starts_with("[senate] refund-v2 的补读：1 条事件"),
        "{text}"
    );
    let (err, text) = mcp.call("post", json!({"body": "进展"})).await;
    assert!(
        !err && text.starts_with("已在 refund-v2 上留言（seq 2）"),
        "{text}"
    );
    let mut ghost = McpClient::start_in(h.socket(), selector("ghost"), Locale::Zh).await;
    h.stop().await;
    let (err, text) = ghost.call("catch_up", json!({})).await;
    assert!(
        err && text.contains("Senate bridge daemon 没有运行"),
        "{text}"
    );
}

/// A client lists the tools once. When the session's agent is put on the
/// Speaker's desk under a running session, the server says the list
/// changed and the next listing has the presiding tools; the same when it
/// is taken off. A session whose agent stays what it was is told nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_whose_role_changes_is_told_its_tools_changed() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("a", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "c1", &wd, None)
        .await
        .unwrap();
    let mut env = h.env_for("x1", None);
    env.agent = Some("codex".into());
    h.hook_with(HookKind::SessionStart, "x1", &wd, &env)
        .await
        .unwrap();
    let presiding = |listed: &[String]| {
        senate_bridge::mcp::PRESIDING_TOOLS
            .iter()
            .filter(|tool| listed.iter().any(|n| n == *tool))
            .count()
    };
    let mut claude = McpClient::start(h.socket(), selector("c1")).await;
    let mut codex = McpClient::start(h.socket(), selector("x1")).await;
    assert_eq!(presiding(&claude.tool_names().await), 0);
    assert_eq!(presiding(&codex.tool_names().await), 0);

    // claude is named the Speaker, and the daemon hears of it (asked for
    // its status it asks the Senate again; a Senate that says its office on
    // the stream needs no asking).
    let learn = || async {
        ipc::request::<Value>(
            &h.socket(),
            &Request::Status,
            Duration::from_secs(1),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
    };
    h.server.set_speaker("claude");
    learn().await;
    claude.list_changed().await;
    assert_eq!(
        presiding(&claude.tool_names().await),
        senate_bridge::mcp::PRESIDING_TOOLS.len()
    );
    // codex is what it was: its session is told nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(presiding(&codex.tool_names().await), 0);
    assert_eq!(codex.list_changes, 0);

    // codex is named instead: claude's session loses the tools, codex's
    // gets them, and each is told once.
    h.server.set_speaker("codex");
    learn().await;
    claude.list_changed().await;
    codex.list_changed().await;
    assert_eq!(presiding(&claude.tool_names().await), 0);
    assert_eq!(
        presiding(&codex.tool_names().await),
        senate_bridge::mcp::PRESIDING_TOOLS.len()
    );
}

/// Codex lists tools once and never again, so a Codex session is given
/// every role's tools from the start, each led by the role it needs; a call
/// without the role is refused, naming who holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_codex_session_lists_every_role_tool_and_is_refused_by_role() {
    let mut h = Harness::new().await;
    h.server.set_speaker("claude");
    h.server.state.inner.lock().unwrap().chair = Some((path("claude"), true));
    h.start().await;
    let wd = h.workdir("a", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "c1", &wd, None)
        .await
        .unwrap();
    let mut env = h.env_for("x1", None);
    env.agent = Some("codex".into());
    h.hook_with(HookKind::SessionStart, "x1", &wd, &env)
        .await
        .unwrap();

    let mut codex = McpClient::start_codex(h.socket(), selector("x1")).await;
    let tools = codex.request("tools/list", json!({})).await;
    let tools = tools["tools"].as_array().unwrap();
    let description = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is listed"))["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for tool in senate_bridge::mcp::CHAIR_TOOLS {
        let text = description(tool);
        assert!(text.starts_with("If you chair this committee: "), "{text}");
        assert!(!text.contains("only the chair's session"), "{text}");
    }
    for tool in senate_bridge::mcp::PRESIDING_TOOLS {
        let text = description(tool);
        assert!(
            text.starts_with("If you work the Speaker's desk: "),
            "{text}"
        );
        assert!(!text.contains("has this tool"), "{text}");
    }
    assert!(tools.iter().any(|t| t["name"] == "spawn"));
    assert!(!description("ask").starts_with("If you"));

    let (err, text) = codex.call("chair_docket", json!({})).await;
    assert!(err);
    assert!(
        text.contains("the chair of `refund-v2` is [senator::claude]"),
        "{text}"
    );
    assert!(text.contains("to reach the chair, ask `chair`"), "{text}");
    let (err, text) = codex
        .call(
            "speaker_route",
            json!({"committee": "refund-v2", "seq": 1, "to": "claude"}),
        )
        .await;
    assert!(err);
    assert!(
        text.contains("[senator::codex] does not work the Speaker's desk"),
        "{text}"
    );
    assert!(text.contains("To reach the desk, ask `speaker`"), "{text}");

    // Any other client is given only the tools of the roles it holds.
    let mut claude = McpClient::start(h.socket(), selector("x1")).await;
    let listed = claude.tool_names().await;
    for tool in senate_bridge::mcp::CHAIR_TOOLS {
        assert!(!listed.iter().any(|n| n == tool), "{tool} in {listed:?}");
    }
}

/// The presiding tools are the presiding session's alone: listed for the
/// session whose agent holds the Speaker's seat, left out for every other
/// session, and refused if called anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_presiding_session_is_given_the_presiding_tools() {
    let mut h = Harness::new().await;
    // A Senate used alone: claude is named its Speaker.
    h.server.set_speaker("claude");
    h.start().await;
    let wd = h.workdir("a", Some("refund-v2"));
    let start = h
        .hook(HookKind::SessionStart, "c1", &wd, None)
        .await
        .unwrap();
    let context = start["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains("[senator::claude] is the Speaker of the Senate [/senate]"),
        "{context}"
    );
    let mut env = h.env_for("x1", None);
    env.agent = Some("codex".into());
    let start = h
        .hook_with(HookKind::SessionStart, "x1", &wd, &env)
        .await
        .unwrap();
    let context = start["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(!context.contains("is the Speaker"), "{context}");

    let names = |tools: &Value| -> Vec<String> {
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let mut presiding = McpClient::start(h.socket(), selector("c1")).await;
    let listed = names(&presiding.request("tools/list", json!({})).await);
    for tool in senate_bridge::mcp::PRESIDING_TOOLS {
        assert!(listed.iter().any(|n| n == tool), "{tool} in {listed:?}");
    }
    assert!(listed.iter().any(|n| n == "ask"));

    let mut codex = McpClient::start(h.socket(), selector("x1")).await;
    let listed = names(&codex.request("tools/list", json!({})).await);
    assert!(listed.iter().any(|n| n == "ask"));
    for tool in senate_bridge::mcp::PRESIDING_TOOLS {
        assert!(!listed.iter().any(|n| n == tool), "{tool} in {listed:?}");
    }
    let (err, text) = codex
        .call(
            "speaker_route",
            json!({"committee": "refund-v2", "seq": 1, "to": "claude"}),
        )
        .await;
    assert!(err);
    assert!(
        text.contains("[senator::codex] does not work the Speaker's desk"),
        "{text}"
    );
    // The daemon refuses the request itself, whatever a client lists.
    let refused = ipc::request::<Posted>(
        &h.socket(),
        &Request::Route {
            session: selector("x1"),
            committee: committee(),
            seq: Seq(1),
            to: path("claude"),
            target_committee: None,
            id: None,
        },
        Duration::from_secs(1),
        Duration::from_secs(5),
    )
    .await
    .unwrap_err();
    let ClientError::Remote(e) = refused else {
        panic!("{refused}")
    };
    assert_eq!(e.code, "not_presiding");

    // An unregistered session cannot acquire a role from the shared harness name or pin.
    let fresh = SenateMcp::new(h.socket(), selector("not-yet"));
    assert!(!fresh.presides().await);
    let pinned =
        SenateMcp::new(h.socket(), selector("not-yet")).with_pins(senate_bridge::mcp::Pins {
            runtime: Runtime::Claude,
            env: Some("codex".into()),
            file: None,
        });
    assert!(!pinned.presides().await);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_to_the_person_is_still_settled_in_the_agents_inbox() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("s1")).await;
    let q = h
        .server
        .append(question(person("alice"), claude("alice"), "which header?"));
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            claude("alice"),
            "Idempotency-Key",
        )
        .to(person("alice"))
        .in_thread(q.seq),
    );
    // Unread for the person (their inbox lists it), settled for the agent:
    // its inbox rule did not change.
    let (_, text) = mcp.call("inbox", json!({})).await;
    assert!(text.starts_with("Nothing open waits on"), "{text}");
    assert!(
        text.ends_with("1 answered thread(s) not shown; all=true lists them."),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn antigravity_hooks_and_mcp_use_the_same_exact_root_session() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("agy", Some("refund-v2"));
    let ids = [
        "9cf71898-7afd-492a-aada-8b05f48a337e",
        "107c866b-314c-4404-856d-c9956fed3e2f",
    ];
    // The same raw id from a different runtime keeps its original record.
    h.hook_codex(HookKind::SessionStart, ids[0], &wd).await;
    for id in ids {
        let input = json!({
            "conversationId": id,
            "cwd": wd,
            "workspacePaths": ["/not-the-effective-cwd"],
            "transcriptPath": wd.join("transcript.jsonl"),
        })
        .to_string();
        let out = senate_bridge::hook::run_as(
            HookKind::SessionStart,
            &h.socket(),
            &input,
            &HookEnv {
                session_id: Some("unrelated".into()),
                ..Default::default()
            },
            Some(Runtime::Agy),
            Default::default(),
        )
        .await
        .unwrap();
        let out: Value = serde_json::from_str(&out).unwrap();
        assert!(
            out["injectSteps"][0]["systemMessage"]["systemMessage"]
                .as_str()
                .unwrap()
                .contains(&format!("agy-{id}")),
            "{out}"
        );
        assert!(
            senate_bridge::hook::run_as(
                HookKind::SessionEnd,
                &h.socket(),
                &input,
                &HookEnv::default(),
                Some(Runtime::Agy),
                Default::default(),
            )
            .await
            .is_none()
        );
        assert!(
            h.store()
                .session(&Runtime::Agy.session_key(id))
                .unwrap()
                .unwrap()
                .is_live()
        );
    }
    assert_eq!(
        h.store().session(ids[0]).unwrap().unwrap().runtime,
        Runtime::Codex
    );

    let mut mcp =
        McpClient::start_for(h.socket(), selector(ids[0]), Locale::En, Runtime::Agy).await;
    // agy lists the tools once for all its conversations: the role tools
    // are there from the start, and refuse a call without the role.
    let listed = mcp.request("tools/list", json!({})).await;
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tool in ["chair_docket", "speaker_docket", "spawn"] {
        assert!(names.contains(&tool), "{tool} in {names:?}");
    }
    let without_root = mcp
        .request(
            "tools/call",
            json!({
                "name": "join", "arguments": {"committee":"other"},
                "_meta":{"threadId":ids[0]},
            }),
        )
        .await;
    assert_eq!(without_root["isError"], true, "{without_root}");
    // An explicit committee cannot turn a missing or unknown session into
    // a sessionless writer using some other senator's identity.
    let count = h.server.events("other").len();
    for metadata in [
        json!({}),
        json!({"antigravity.google/root_conversation_id":"00000000-0000-4000-8000-000000000001"}),
    ] {
        let post = mcp
            .request(
                "tools/call",
                json!({
                    "name":"post", "arguments":{"committee":"other","body":"identity check"},
                    "_meta":metadata
                }),
            )
            .await;
        assert_eq!(post["isError"], true, "{post}");
    }
    assert_eq!(h.server.events("other").len(), count);
    let joined = mcp
        .request(
            "tools/call",
            json!({
                "name": "join", "arguments": {"committee":"other"},
                "_meta":{
                    "antigravity.google/root_conversation_id": ids[1],
                    "threadId":ids[0],
                },
            }),
        )
        .await;
    assert_ne!(joined["isError"], true, "{joined}");
    let committee = |id: &str| {
        h.store()
            .session(id)
            .unwrap()
            .unwrap()
            .committee()
            .unwrap()
            .to_string()
    };
    assert_eq!(committee(ids[0]), "refund-v2");
    assert_eq!(committee(&Runtime::Agy.session_key(ids[0])), "refund-v2");
    assert_eq!(committee(&Runtime::Agy.session_key(ids[1])), "other");

    let wrong_role = ipc::request::<Value>(
        &h.socket(),
        &Request::ChairDocket {
            session: selector(&Runtime::Agy.session_key(ids[1])),
        },
        WAIT,
        WAIT,
    )
    .await;
    assert!(matches!(
        wrong_role,
        Err(ClientError::Remote(e)) if e.code == ipc::codes::NOT_CHAIRING
    ));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_search_shows_what_the_committee_knows_with_its_sources() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let mut mcp = McpClient::start(h.socket(), selector("s1")).await;
    let point = |text: &str, refs: Vec<Seq>, supersedes: Vec<Seq>| {
        NewEvent::new(committee(), Kind::Note, Address::Speaker, text).with_meta(
            meta::MEMORY,
            senate_core::MemoryPayload::Point {
                refs,
                supersedes,
                digest: None,
            }
            .to_value(),
        )
    };
    h.server
        .append(point("Deploys happen at noon", vec![Seq(1)], vec![]));
    h.server
        .append(point("Deploys happen at night", vec![Seq(2)], vec![Seq(1)]));

    let (err, text) = mcp.call("memory_search", json!({"query": "deploys"})).await;
    assert!(!err, "{text}");
    assert!(
        text.starts_with("[senate] What refund-v2 knows (1 point(s)"),
        "{text}"
    );
    assert!(
        text.contains("- seq 2 · Deploys happen at night (from seq 2)"),
        "{text}"
    );
    assert!(!text.contains("noon"), "{text}");

    let (err, text) = mcp
        .call("memory_search", json!({"query": "deploys", "all": true}))
        .await;
    assert!(!err, "{text}");
    assert!(
        text.contains("- seq 1 · Deploys happen at noon (from seq 1) · replaced by seq 2"),
        "{text}"
    );
    let (_, text) = mcp.call("memory_search", json!({"query": "billing"})).await;
    assert!(
        text.contains("Nothing refund-v2 remembers matches"),
        "{text}"
    );
    h.stop().await;
}

fn discover_meta(client_name: Option<&str>) -> Value {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "io.modelcontextprotocol/protocolVersion".into(),
        json!("2026-07-28"),
    );
    meta.insert(
        "io.modelcontextprotocol/clientCapabilities".into(),
        json!({}),
    );
    if let Some(name) = client_name {
        meta.insert(
            "io.modelcontextprotocol/clientInfo".into(),
            json!({"name": name, "version": "0.172.0"}),
        );
    }
    Value::Object(meta)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_discover_exposes_desk_and_spawn_tools_for_secretary_and_refuses_ordinary_agent() {
    use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
    use senate_bridge::ipc::{self, HookReply, HostedRuntime, SpawnReply, StopReply};
    use std::os::unix::fs::PermissionsExt as _;

    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let program = h.dir.path().join("fixture-tui");
    std::fs::write(
        &program,
        "#!/bin/sh\nwhile [ -d \"$PWD\" ]; do sleep 0.1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            program.to_str().unwrap()
        ),
    )
    .unwrap();
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            env!("CARGO_BIN_EXE_senate").into(),
        )
        .await
        .unwrap(),
    );

    let project = h.workdir("other-proj", None);
    let office_dir = h.workdir("office-proj", Some("office"));
    // Seed a hosted chair on `other`, then stop it so it needs restoring.
    let chair: SpawnReply = ipc::request(
        &h.socket(),
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: "other".parse().unwrap(),
            alias: Some(path("main")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let _: StopReply = ipc::request(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: "other::main".parse().unwrap(),
            force: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert_eq!(
        h.store().session(&chair.session_id).unwrap().unwrap().state,
        senate_bridge::SessionState::Ended
    );

    // Register an existing/resumed Codex secretary and an ordinary fresh Codex
    // session on `office`.
    let codex_hook = |id: &str, source: &str| {
        Request::SessionHook(
            serde_json::from_value(json!({
                "event": "session_start",
                "runtime": "codex",
                "session_id": id,
                "cwd": office_dir,
                "committee_file": "office",
                "source": source,
            }))
            .unwrap(),
        )
    };
    let _: HookReply = ipc::request(
        &h.socket(),
        &codex_hook("sec-resumed", "resume"),
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let _: HookReply = ipc::request(
        &h.socket(),
        &codex_hook("ordinary-fresh", "startup"),
        WAIT,
        WAIT,
    )
    .await
    .unwrap();

    let sec_agent = path("codex-sec-resumed");
    {
        let mut server = h.server.state.inner.lock().unwrap();
        server.chair = Some((chair.senator.clone(), true));
        server
            .roster
            .name_speaker(Some(path("alice").into()))
            .unwrap();
        server
            .roster
            .set_secretaries(path("alice"), vec![sec_agent.clone()])
            .unwrap();
    }
    let _: Value = ipc::request(&h.socket(), &Request::Status, WAIT, WAIT)
        .await
        .unwrap();

    // 1. Legacy Codex config (`args = ["mcp"]` -> default Runtime::Claude) over
    // MCP 2026-07-28 `server/discover`: `tools/list` carries no threadId and
    // no `initialize` happened, so client identity comes from `_meta`.
    let mut codex = McpClient::start_discover(
        h.socket(),
        SessionSelector::default(),
        Runtime::Claude,
        Some("codex-mcp-client"),
    )
    .await;
    let listed = codex
        .request(
            "tools/list",
            json!({ "_meta": discover_meta(Some("codex-mcp-client")) }),
        )
        .await;
    let tools = listed["tools"].as_array().unwrap();
    let desc = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing from Codex discover tools/list"))
            ["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    for name in senate_bridge::mcp::PRESIDING_TOOLS {
        assert!(desc(name).starts_with("If you work the Speaker's desk: "));
    }
    for name in senate_bridge::mcp::CHAIR_TOOLS {
        assert!(desc(name).starts_with("If you chair this committee: "));
    }
    let spawn_desc = desc("spawn");
    assert!(
        spawn_desc.starts_with("If you work the Speaker's desk or chair this committee: "),
        "{spawn_desc}"
    );
    assert!(
        spawn_desc.contains("restore a recorded local senator's session"),
        "{spawn_desc}"
    );
    assert!(
        !spawn_desc.contains(
            "restoring a hosted session, or stopping its terminal requires local presence"
        ),
        "{spawn_desc}"
    );

    // 2. Explicit `senate mcp --runtime codex` also exposes the full tool set
    // even when optional `clientInfo` is omitted from `_meta`.
    let mut codex_explicit =
        McpClient::start_discover(h.socket(), SessionSelector::default(), Runtime::Codex, None)
            .await;
    let explicit_listed = codex_explicit
        .request("tools/list", json!({ "_meta": discover_meta(None) }))
        .await;
    let explicit_names: Vec<&str> = explicit_listed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for required in ["speaker_docket", "speaker_forward", "spawn", "chair_docket"] {
        assert!(
            explicit_names.contains(&required),
            "{required} missing from explicit codex runtime list: {explicit_names:?}"
        );
    }

    // 3. Non-Codex MCP 2026-07-28 client keeps role-filtered tool lists.
    let mut non_codex_ordinary = McpClient::start_discover(
        h.socket(),
        selector("ordinary-fresh"),
        Runtime::Claude,
        Some("claude-code"),
    )
    .await;
    let ordinary_names: Vec<String> = non_codex_ordinary
        .request(
            "tools/list",
            json!({ "_meta": discover_meta(Some("claude-code")) }),
        )
        .await["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    for forbidden in ["speaker_docket", "speaker_forward", "spawn", "chair_docket"] {
        assert!(
            !ordinary_names.iter().any(|n| n == forbidden),
            "ordinary non-Codex client got {forbidden}: {ordinary_names:?}"
        );
    }
    let mut non_codex_secretary = McpClient::start_discover(
        h.socket(),
        selector("sec-resumed"),
        Runtime::Claude,
        Some("claude-code"),
    )
    .await;
    let sec_names: Vec<String> = non_codex_secretary
        .request(
            "tools/list",
            json!({ "_meta": discover_meta(Some("claude-code")) }),
        )
        .await["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_owned))
        .collect();
    for expected in ["speaker_docket", "speaker_forward", "spawn"] {
        assert!(
            sec_names.iter().any(|n| n == expected),
            "secretary non-Codex client missed {expected}: {sec_names:?}"
        );
    }
    assert!(!sec_names.iter().any(|n| n == "chair_docket"));

    // Queue a cross-committee implementation request on `office` meant for
    // `other::chair` (mirroring office#101).
    let queued = h.server.append(
        NewEvent::new(
            "office".parse().unwrap(),
            Kind::Request,
            Address::senator(&sec_agent),
            "Please fix the regression on other.",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, json!("/senate::other::chair")),
    );

    let call_as = |thread_id: &str, name: &str, arguments: Value| {
        let mut meta = discover_meta(Some("codex-mcp-client"));
        meta.as_object_mut()
            .unwrap()
            .insert("threadId".into(), json!(thread_id));
        json!({ "name": name, "arguments": arguments, "_meta": meta })
    };
    let text_of = |reply: &Value| {
        reply["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };

    // Ordinary Codex agent is refused desk and restore calls on the shared server.
    for (tool, args, expected) in [
        (
            "speaker_docket",
            json!({}),
            "does not work the Speaker's desk",
        ),
        (
            "speaker_forward",
            json!({"committee": "office", "seq": queued.seq.0}),
            "does not work the Speaker's desk",
        ),
        (
            "spawn",
            json!({"action": "restore", "senator": "other::main"}),
            "only the Speaker's office or this committee's chair",
        ),
    ] {
        let reply = codex
            .request("tools/call", call_as("ordinary-fresh", tool, args))
            .await;
        assert_eq!(reply["isError"], json!(true), "{tool}: {reply}");
        assert!(
            text_of(&reply).contains(expected),
            "{tool}: {}",
            text_of(&reply)
        );
    }

    // Resumed Codex secretary sees the queued item, forwards it with Speaker
    // attribution (`acted_by`), and restores the ended chair via `spawn`.
    let docket = codex
        .request(
            "tools/call",
            call_as("sec-resumed", "speaker_docket", json!({})),
        )
        .await;
    assert_ne!(docket["isError"], json!(true), "{docket}");
    let docket_text = text_of(&docket);
    assert!(
        docket_text.contains("Please fix the regression on other."),
        "{docket_text}"
    );

    let forwarded = codex
        .request(
            "tools/call",
            call_as(
                "sec-resumed",
                "speaker_forward",
                json!({"committee": "office", "seq": queued.seq.0}),
            ),
        )
        .await;
    assert_ne!(forwarded["isError"], json!(true), "{forwarded}");
    let other_events = h.server.events("other");
    let relayed = other_events
        .iter()
        .find(|e| e.body == "Please fix the regression on other.")
        .expect("forwarded request landed on other");
    assert_eq!(relayed.from, Address::Speaker);
    assert_eq!(relayed.meta.get(meta::ACTED_BY), Some(&json!(&sec_agent)));

    let restored = codex
        .request(
            "tools/call",
            call_as(
                "sec-resumed",
                "spawn",
                json!({"action": "restore", "senator": "other::main"}),
            ),
        )
        .await;
    assert_ne!(restored["isError"], json!(true), "{restored}");
    let restored_reply: SpawnReply = serde_json::from_str(&text_of(&restored)).unwrap();
    assert_eq!(restored_reply.senator, chair.senator);
    assert!(
        h.store()
            .session(&chair.session_id)
            .unwrap()
            .unwrap()
            .is_live()
    );
    let restore_note = h
        .server
        .events("other")
        .into_iter()
        .find(|e| e.body.contains("restored") && e.body.contains(chair.senator.as_str()))
        .expect("restore audit note recorded");
    assert_eq!(restore_note.from, Address::Speaker);
    assert_eq!(
        restore_note.meta.get(meta::ACTED_BY),
        Some(&json!(&sec_agent))
    );

    // A fresh Codex session newly appointed to the desk uses the same
    // already-opened shared `server/discover` server immediately.
    let _: HookReply = ipc::request(&h.socket(), &codex_hook("sec-fresh", "startup"), WAIT, WAIT)
        .await
        .unwrap();
    let fresh_agent = path("codex-sec-fresh");
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .set_secretaries(path("alice"), vec![sec_agent.clone(), fresh_agent])
        .unwrap();
    let _: Value = ipc::request(&h.socket(), &Request::Status, WAIT, WAIT)
        .await
        .unwrap();
    let fresh_docket = codex
        .request(
            "tools/call",
            call_as("sec-fresh", "speaker_docket", json!({})),
        )
        .await;
    assert_ne!(fresh_docket["isError"], json!(true), "{fresh_docket}");

    // Clean up the restored fixture host before stopping the harness.
    let _: StopReply = ipc::request(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: "other::main".parse().unwrap(),
            force: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_person_corrects_and_deletes_memory_in_their_own_name() {
    use senate_bridge::ipc::{self, Posted, Request};
    let mut h = Harness::new().await;
    h.start().await;
    let edit = |targets: Vec<Seq>, text: Option<&str>| Request::MemoryEdit {
        session: Default::default(),
        committee: Some(committee()),
        targets,
        text: text.map(Into::into),
        id: None,
    };
    let posted: Posted = ipc::request(
        &h.socket(),
        &edit(vec![Seq(7)], Some("Deploys happen at night")),
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let event = h.server.last_event("refund-v2");
    assert_eq!(event.seq, posted.seq);
    assert_eq!(event.kind, Kind::Note);
    assert_eq!(event.body, "Deploys happen at night");
    assert!(event.on_behalf_of.is_none());
    assert_eq!(
        senate_core::MemoryPayload::of(&event),
        Some(senate_core::MemoryPayload::Correct {
            target: Seq(7),
            refs: vec![]
        })
    );
    let _: Posted = ipc::request(&h.socket(), &edit(vec![Seq(7), Seq(9)], None), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(
        senate_core::MemoryPayload::of(&h.server.last_event("refund-v2")),
        Some(senate_core::MemoryPayload::Delete {
            targets: vec![Seq(7), Seq(9)]
        })
    );
    // A correction names one point.
    let refused: Result<Posted, _> = ipc::request(
        &h.socket(),
        &edit(vec![Seq(7), Seq(9)], Some("x")),
        WAIT,
        WAIT,
    )
    .await;
    assert!(refused.is_err());
    h.stop().await;
}

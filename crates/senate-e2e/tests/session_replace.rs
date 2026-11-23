//! A replacement through the CLI, a bridge hosting two real terminals,
//! and the HTTP server that owns the committee's roles and log.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{HostState, HostedRuntime, Request, SessionsReply, SpawnReply};
use senate_bridge::{BridgeConfig, Paths, testenv};
use senate_core::{Address, Hub, Kind, Locale, NewEvent};
use std::os::unix::fs::PermissionsExt;

const CLI: &str = env!("CARGO_BIN_EXE_senate-e2e-cli");

struct Terminals(Paths);
impl Drop for Terminals {
    fn drop(&mut self) {
        senate_bridge::host::kill_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replacing_a_hosted_chair_transfers_its_alias_and_prompt_on_the_server() {
    let world = Setup::new()
        .without_speaker()
        .server(|c| c.roster.speaker = Some("senator::alice".parse().unwrap()))
        .locale(Locale::En)
        .start()
        .await;
    world
        .hub()
        .append(NewEvent::new(
            committee(),
            Kind::Note,
            person("alice"),
            "Open the committee",
        ))
        .await
        .unwrap();
    // PTY hosts have several socket names below this root. Keep them within
    // sockaddr_un's limit even on a Mac with a long per-user TMPDIR.
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let fixture = root.path().join("fixture-claude");
    std::fs::write(&fixture, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut desk = Desk::new(
        root.path(),
        "alice",
        Locale::En,
        &format!("[companion]\nclaude_bin = {:?}", fixture),
    );
    let laptop = &mut desk.laptop;
    laptop.config = BridgeConfig::new(&world.server_url(), "tok-alice", name("alice"));
    laptop.config.locale = Locale::En;
    laptop.config.committees.push(committee());
    let _cleanup = Terminals(laptop.paths.clone());
    laptop.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: laptop.paths.clone(),
                config: laptop.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            CLI.into(),
        )
        .await
        .unwrap(),
    );

    let old: SpawnReply = laptop
        .ipc(Request::Spawn {
            session: Default::default(),
            runtime: HostedRuntime::Claude,
            committee: committee(),
            alias: Some(path("reviewer")),
            cwd: laptop.work.clone(),
            chair: true,
            secretary: false,
            prompt: None,
            model: Default::default(),
        })
        .await;
    world.wait_connected(old.senator.as_str(), true).await;
    assert_eq!(
        world.hub().chair(&committee()).await.unwrap().chair,
        Some(old.senator.clone())
    );
    let prompt = "Continue the gateway review";
    let mut cmd = tokio::process::Command::new(CLI);
    let out = testenv::isolate_async(&mut cmd, root.path())
        .env("SENATE_HOME", laptop.paths.home())
        .args(["session", "replace", "reviewer", "--prompt", prompt])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let sessions: SessionsReply = laptop.ipc(Request::Sessions).await;
    assert_eq!(sessions.rows.len(), 2);
    let kept = sessions
        .rows
        .iter()
        .find(|r| r.primary != old.senator)
        .unwrap();
    let retired = sessions
        .rows
        .iter()
        .find(|r| r.primary == old.senator)
        .unwrap();
    assert_eq!(retired.state, HostState::Stopped);
    assert_eq!(kept.state, HostState::Running);
    let aliases = Hub::aliases(world.hub().as_ref()).await.unwrap();
    assert_eq!(
        aliases.alias_of(&committee(), &kept.primary),
        Some(&path("reviewer"))
    );
    assert_eq!(aliases.alias_of(&committee(), &old.senator), None);
    assert_eq!(
        world
            .hub()
            .chair(&committee())
            .await
            .unwrap()
            .chair
            .as_ref(),
        Some(&kept.primary)
    );
    let events = world.events_of(&committee()).await;
    let prompts: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Request && e.body == prompt)
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].to, [Address::senator(&kept.primary)]);
    world.wait_connected(old.senator.as_str(), false).await;
    world.wait_connected(kept.primary.as_str(), true).await;
    laptop.close().await;
    world.stop().await;
}

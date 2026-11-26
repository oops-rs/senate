//! A replacement through the CLI, a bridge hosting two real terminals,
//! and the HTTP server that owns the committee's roles and log.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::*;
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{
    HostState, HostedRuntime, Request, RosterCommittee, SessionsReply, SpawnReply,
};
use senate_bridge::{BridgeConfig, testenv};
use senate_core::alias::AliasName;
use senate_core::{Address, Hub, Kind, Locale, NewEvent, SenatorName};
use std::os::unix::fs::PermissionsExt;

const CLI: &str = env!("CARGO_BIN_EXE_senate-e2e-cli");

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
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
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
    cmd.kill_on_drop(true);
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
    // Show the old identity only when explicitly requested. Its retired
    // alias belongs to the new chair and the stopped host stays restorable
    // by the original ID when otherwise eligible.
    let list = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new(CLI);
        cmd.kill_on_drop(true);
        testenv::isolate_async(&mut cmd, root.path())
            .env("SENATE_HOME", laptop.paths.home())
            .args(args);
        cmd
    };
    let listing = list(&["session", "list"]).output().await.unwrap();
    assert!(listing.status.success(), "{listing:?}");
    let listing = String::from_utf8(listing.stdout).unwrap();
    let old_display = senate_core::alias::short_name(&old.senator);
    assert!(listing.contains("reviewer"));
    assert!(!listing.contains(&old_display));
    let listing = list(&["session", "list", "--all"]).output().await.unwrap();
    assert!(listing.status.success(), "{listing:?}");
    assert!(
        String::from_utf8(listing.stdout)
            .unwrap()
            .contains(&old_display)
    );
    let listing = list(&["session", "list", "--all", "-v"])
        .output()
        .await
        .unwrap();
    assert!(listing.status.success(), "{listing:?}");
    assert!(
        String::from_utf8(listing.stdout)
            .unwrap()
            .contains(old.senator.as_str())
    );

    // `senate alias` is the person's: refused from a coding session's
    // process tree (a test may run inside one), so asked as the CLI asks.
    let _: SenatorName = laptop
        .ipc(Request::SetAlias {
            senator: AliasName::bare(kept.primary.clone()),
            alias: "lead".parse().unwrap(),
            committee: None,
        })
        .await;
    let before: Vec<RosterCommittee> = laptop.ipc(Request::Roster).await;
    let pending_before = before
        .iter()
        .find(|row| row.committee == committee())
        .unwrap()
        .pending;
    let question = world
        .hub()
        .append(
            NewEvent::new(
                committee(),
                Kind::Question,
                person("bob"),
                "Which review is next?",
            )
            .to(person("alice"))
            .to(Address::senator(&kept.primary)),
        )
        .await
        .unwrap();
    world
        .hub()
        .append(
            NewEvent::new(
                committee(),
                Kind::Answer,
                Address::senator(&kept.primary),
                "The gateway review.",
            )
            .in_thread(question.event.seq)
            .to(person("alice")),
        )
        .await
        .unwrap();
    let inbox = list(&["inbox", "--committee", committee().as_str()])
        .output()
        .await
        .unwrap();
    assert!(inbox.status.success(), "{inbox:?}");
    assert!(
        String::from_utf8(inbox.stdout)
            .unwrap()
            .contains("lead answered")
    );
    let notes = senate_core::CommitteeId::new("notes").unwrap();
    world
        .hub()
        .append(NewEvent::new(
            notes.clone(),
            Kind::Note,
            person("alice"),
            "Open a second committee",
        ))
        .await
        .unwrap();
    let roster: Vec<RosterCommittee> = laptop.ipc(Request::Roster).await;
    let committee_row = roster
        .iter()
        .find(|row| row.committee == committee())
        .unwrap();
    assert_eq!(committee_row.chair, Some(kept.primary.clone()));
    assert_eq!(committee_row.chair_display_name.as_deref(), Some("lead"));
    assert!(committee_row.chair_live);
    assert_eq!(committee_row.pending, pending_before + 1);
    assert!(
        roster
            .iter()
            .any(|row| row.committee == notes && row.members.is_empty() && row.pending == 0)
    );
    assert_eq!(committee_row.members.iter().filter(|m| m.active).count(), 1);
    let overview = list(&["committees"]).output().await.unwrap();
    assert!(overview.status.success(), "{overview:?}");
    assert!(String::from_utf8(overview.stdout).unwrap().contains("lead"));

    world
        .hub()
        .set_secretaries(path("alice"), vec![kept.primary.clone()])
        .await
        .unwrap();
    // Exercise both CLI role flags through the real server. The alias-less
    // launches select successive available names on the same committee.
    for (role, alias) in [("--chair", "claude1"), ("--secretary", "claude2")] {
        let output = list(&[
            "spawn",
            "claude",
            "--committee",
            committee().as_str(),
            "--dir",
            laptop.work.to_str().unwrap(),
            role,
        ])
        .output()
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), alias);
    }
    let aliases = Hub::aliases(world.hub().as_ref()).await.unwrap();
    let clerk = aliases.get(&committee(), &path("claude2")).unwrap();
    assert_eq!(
        world.hub().secretaries().get(&path("alice")).unwrap(),
        &[kept.primary.clone(), clerk.clone()]
    );
    let chair = world.hub().chair(&committee()).await.unwrap();
    assert_eq!(
        chair.chair.as_ref(),
        aliases.get(&committee(), &path("claude1"))
    );
    laptop.close().await;
    world.stop().await;
}

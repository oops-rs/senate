# Default secretary — parked design (not implemented)

Status: on hold at Wendell's request (thread 2092, seq 2097). Nothing built.
Branch feat/default-secretary from 349356f8.

## Sources
- Design: seq 2094. Chair's conditions: seq 2095. Wendell's answers: seq 2096.

## Decisions (Wendell, seq 2096)
- Q1 runtime: first configured runtime with its default model, overridable by `[local] secretary_profile` (a launch profile name).
- Q2 start: on first use (EnsureSecretary from `senate secretary ask` or the app's first send); restored on first use after a restart; never at daemon start.
- Q3 directories: any directory the person names; no list (spawn already allows any dir: daemon/hosts.rs ~l.737).
- Q4 only the person's ask starts it; an agent's desk item never starts a secretary that is not running. Once running it works the desk like any desk worker. Keep this as ONE predicate.
- Q5 (chair) home committee `office`, alias `secretary`. An existing `office` committee is used as it is; an existing seat aliased `secretary` that is a desk worker IS the default secretary; one that is not → ensure fails naming the conflict.

## Design (seq 2094, with seq 2095 conditions)
- One hosted coding agent on the Speaker's desk, alias `secretary`, committee `office` (opened through the existing OpenCommittee path), cwd $SENATE_HOME/secretary.
- Request::EnsureSecretary (person's CLI only), idempotent under a lock: recorded+running → no-op; paused or not running → restore; not recorded → spawn (secretary=true, prompt = role brief); stopped on purpose → refuse, say how to restore.
- New presiding tool `open_committee {committee, cwd, chair:{profile|runtime, alias, prompt}, members?}`, desk workers only: OpenCommittee as the Speaker with ACTED_BY, then the existing spawn chair=true. It refuses an existing committee (adding a chair or members goes through plain `spawn`). Creating twice must not post a second opening note or replace a chair. Document what a partial failure leaves behind (committee open, chair failed) and make a retry complete it.
- The role brief is a short file in the repo: one committee per project, each chair spawned with a self-contained brief, a secretary_work item per committee, names reported to the person, follow-up through the chairs, and never acting on a peer's request to spawn or create.
- CLI `senate secretary ask "<text>"`: ensure, then a request from the person to senator::secretary in `office`.
- `senate setup doctor` reports the secretary's state: not recorded / running / paused / stopped on purpose.
- `senate setup` writes `secretary_profile` commented out and appends `secretary` to `[local] secretaries` only when that list is empty.
- App (slice 4): pinned "Secretary" sidebar row → SessionPage; first send ensures it. Waits for `app` to merge thread 2038 (MainWindow.swift, SessionPage.swift).
- Left out: menu-bar entry, allowed-dir list, idle auto-pause, team server, more than one default.

## Findings for whoever resumes
- The `[secretary]` table name collides: senated/assembly.rs reads `[secretary]` as compat config for the built-in session. Hence `[local] secretary_profile`.
- "Stopped on purpose" is NOT recorded today. state.rs only has `hosted_paused`, and Stop just clears the pause (host_policy.rs stop path). Ensure and doctor need a small marker, set by an explicit Stop and cleared on restore.
- "First configured runtime": the programs are configured as `[companion] claude_bin`, `[codex] bin` and `[agy] bin` in config.toml (service.rs AGENTS, l.596). Take the first that is set, in that order.
- Person CLI auth: may_appoint only when the person is the Speaker (host_policy.rs authorize_host_action). The ensure inherits that.
- The fake server in tests/support/common.rs has a real senate_core Roster (office, set_secretaries), usable for desk-worker tests.

## Slices
1. Daemon/IPC: config key, home dir, stopped-on-purpose marker, EnsureSecretary, tests under an isolated SENATE_HOME.
2. `open_committee` presiding tool + authority tests (desk worker yes; chair or member no; ACTED_BY audit).
3. CLI `secretary ask`, setup template, doctor, README, role brief file.
4. App, after thread 2038 merges.

# Local IPC caller authorization

A Unix peer UID identifies an account. A coding agent running under that
account can change its name, terminal, and process parent. Senate uses hosted
lineage for routine commands, independent app consent for selected operations,
and process identity for session claims. Trusted files, protected Keychain
items, trusted UI control, debugger access, and administrator access are outside
this threat model.

## Routine commands

Keep the routine CLI flow for Ghostty, Terminal, iTerm, tmux, SSH, and the
terminal attach viewer. The viewer sends input to the host. Commands executed
inside the terminal still have the host's lineage. The paired-machine transport
keeps its own authentication and operation allowlist.

- **macOS.** Start hosts in a verified launchd resource coalition and record the
  coalition before any runtime code can run. XNU retains the label across fork,
  exec, and reparenting. Keep boot-scoped labels after host exit and daemon
  restart. Bind a guarded request to a Mach message audit trailer and query the
  coalition using its process generation. Missing evidence fails closed. A
  shell-started daemon needs a dedicated launcher so the user's terminal does
  not share the host label. The coalition query uses version-sensitive SPI.
- **Linux and WSL.** A protected launcher places each host in a cgroup v2 scope
  before runtime execution. An administrator-owned subtree prevents migration.
  Read membership through a stable peer process handle. Where this facility is
  absent, hosting continues with advisory ancestry checks. Startup, `senate
  status`, and `senate setup doctor` surface that mode. The proposed `senate
  setup guard` would install `/usr/local/libexec/senate-host-guard`, protect
  `/sys/fs/cgroup/senate-hosted/<uid>`, and add a launch-only rule in
  `/etc/sudoers.d/99-senate-host-guard`. The daemon never installs it.

Lineage covers processes born in the hosted scope. An independent scheduler can
launch in a separate scope. A native agent can also detach from the user's
terminal. In both cases the routine ancestry check may allow the caller. Runtime
names and ancestry provide additional rejection while the original tree is visible.

| Policy target | Hosted descendants | Native agents | Lines and rebind | Hooks |
| --- | --- | --- | --- | --- |
| macOS with signed app | Enforced by coalition | Routine checks advisory | App consent enforced | Process binding and role reauthorization enforced |
| macOS headless | Enforced by coalition | Routine checks advisory | Refused | Process binding and role reauthorization enforced |
| Linux/WSL with protected scope | Enforced by cgroup | Routine checks advisory | Refused | Process binding and role reauthorization enforced |
| Linux/WSL without protected scope | Routine checks advisory | Routine checks advisory | Refused | Privileged claims require stable proof |

The macOS bridge checks one-time proofs for operator requests. It binds each
proof to the exact request digest and the Mach sender's process generation.
Managed launchd hosting persists its daemon coalition before publishing the
bridge. Scope coverage for the shared Codex backend remains advisory until
the dedicated backend lands. Linux and WSL hosting remains advisory. Hook
process queries and person-name reservation are in place. Persistent role
reauthorization is pending. The app's signed XPC queue provides consent for
rebind. Lines issuance awaits the dedicated Observe registry capability.

From an operator terminal in this worktree, with the signed fixture binaries
built from the same revision, run:

```sh
python3 deploy/check-person-terminal.py target/person-guard-scope-fixture
```

Expect `PASS: person terminal accepted; isolated grant list is empty`.
The check uses a fresh home and loopback port, and unloads the disposable
launchd job on exit.

## Codex backend

The app-server executes tools, so on enforced platforms every hosted Codex
needs its own protected backend whose lifetime follows the host. Preserve native
`codex.listen`. Report existing shared hosts as advisory in doctor, status and
session list until restore. The explicit `[codex] hosted_app_server = "shared"`
opt-out also stays advisory. Notify when hosted mode bypasses `codex.listen`.
Verify approvals, turn start and steer, timelines, and Observe on the dedicated
connection. Its connection ownership can support future context-switch handling.

This Mac's three isolated, initialized app-servers used a median of 54.3 MiB
idle RSS each. An independent server resumed the same seeded test thread and
canonical project directory using the same private `CODEX_HOME`. Dedicated
Senate hosting and restore still need end-to-end acceptance.

## Signed app consent

Both Observe lines policies and session rebind need one explicit app decision.
The CLI waits while the app displays the immutable request. The queue has a
short expiry and bounded capacity. A decision consumes the request once, and
requester disconnect cancels it. The original connection receives the result.
Client approval flags carry no authority. Routine permission answers, desk
decisions, archive, metadata grants, list and revoke use the routine path.

XPC checks each message against the local UID and the requirement
`anchor apple generic and identifier "dev.senate.app" and certificate leaf[subject.OU] = "TEAM_ID"`.
The app pins `dev.senate.daemon` and the same team. The signed daemon's
LaunchAgent registers the approval service using a hash of its canonical Senate
home. Debug builds can use Apple Development with the same pinned
`SENATE_PERSON_TEAM_ID` across the CLI, daemon and app core. An isolated signing
fixture accepted Apple Development and refused an ad-hoc signature.

XNU's Unix peer token resolves the socket's last PID when queried. Per-message
Mach attestation is required for process generations and XPC sender identity.

## Session claims

Bind hook and role actions to the runtime's verified process identity, including
its generation. Reserve person names and reject another live session's seat.
Treat self-chosen names as ordinary aliases. Chair, secretary, and Speaker
authority requires a daemon-controlled launch or a person-approved binding. A
native privileged seat moving to a different process requires reauthorization.
Resolve a selector once and carry that proof through dispatch. Derive Git
context from verified process or launch state. Observe requires runtime-owned
transcript paths and matching native record IDs.

## Ablation

- Without a protected lifetime label, reparenting removes the ancestry signal.
  Writable migration controls or clearing labels on host exit defeat it again.
- Without an authenticated app and exact one-time binding, routine ancestry
  gaps also affect sensitive acts, or one click could authorize multiple acts.
- Without process-generation ownership, session IDs and aliases become usable
  as credentials. Path and record checks protect independent transcript inputs.

Platform references include [XNU coalitions](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/coalitions.md),
[XNU Unix socket credentials](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c),
[Apple XPC per-message requirements](https://developer.apple.com/documentation/xpc/xpc_connection_set_peer_lightweight_code_requirement(_:_:)),
and [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html).

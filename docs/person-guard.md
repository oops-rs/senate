# Local IPC caller authorization

A Unix peer UID identifies a local account. A process's current parent, its
terminal, and the name it runs under can change. Senate distinguishes its hosted
process trees, authenticates privileged session claims, and requires independent
person confirmation for the few operations below. This assumes agents cannot
read or change trusted state, access protected Keychain items, control the app's
UI, attach a debugger to the trusted processes, or obtain administrator access.

## Hosted lineage and routine commands

Require same-account peer credentials and refuse a caller in any hosted lineage,
even after the original host exits. Use one check for all person-only operations,
including desk decisions, remote administration, streams and attachments. Keep the
existing ancestry and runtime checks as an additional rejection signal. An allowed
routine command runs directly from a personal terminal, tmux, or SSH. The attach
viewer sends input to the host. CLI commands run inside that terminal still have
the hosted lineage. The paired-machine transport retains its own authentication
and command allowlist.

- **macOS.** Use the kernel resource coalition as the hosted lineage label. XNU
  documents inheritance across fork, exec and ordinary spawn, including after
  reparenting. Launch hosts from a dedicated launchd job with a verified separate
  coalition before any runtime code can run. A shell-started daemon must use
  that dedicated launcher for hosting. Persist coalition labels before releasing
  the runtime, bind them to the current boot, and keep them for that boot even
  when a host exits or the daemon restarts. Bind each guarded request to the
  kernel audit trailer of a Mach message and look up its coalition with an
  identity-version check. The CLI performs that exchange without a prompt for
  routine commands. Fail closed when peer identity or the coalition query is
  unavailable. The `proc_pidinfo` coalition flavor is an SPI and needs OS-version
  verification. A bare PID is insufficient.
- **Linux and WSL.** Place each hosted session in a cgroup v2 scope before
  starting runtime code. An administrator-owned supervisor creates the scopes
  and restricts migration into and out of the protected subtree. A regular
  same-user systemd scope alone cannot establish that invariant. Verify the
  peer's membership against that protected subtree using a stable peer process
  handle. Without these primitives, Linux and WSL continue to host with the
  existing advisory checks. The daemon logs that mode at startup and reports
  it in `senate status` and `senate setup doctor`. `senate setup guard` is an
  explicit, sudo-run provisioning step for the root-owned supervisor, its
  cgroup v2 subtree and its limited launch interface. It is never automatic.
  SSH adds no lineage exemptions.

Scope and coalition checks identify processes born inside the hosted lineage.
A scheduler or service that independently launches a process in another lineage
can lose the origin of the request. The same is true of a native agent whose
process detaches from a person's terminal. Such callers may pass the routine
check. Separate containment of those broker interfaces is required if their
use must be prohibited. Runtime ancestry still catches known native agents
while the original process chain remains readable.

The table describes the target behavior once the proposed controls are in place.

| Platform | Hosted descendants | Native agents | App confirmation | Hooks |
| --- | --- | --- | --- | --- |
| macOS, signed app available | Enforced by coalition | Advisory for routine requests | Enforced for lines and rebind | Enforced process binding and role reauthorization |
| macOS, headless or without a trusted app | Enforced by coalition | Advisory for routine requests | Refused for lines and rebind | Enforced process binding and role reauthorization |
| Linux or WSL, provisioned scope and stable process handles | Enforced by cgroup | Advisory for routine requests | Refused for lines and rebind | Enforced process binding and role reauthorization |
| Linux or WSL, without a protected scope | Advisory for routine requests | Advisory for routine requests | Refused for lines and rebind | Enforced for privileged hooks with stable process proof, otherwise refused |

Hosted Codex needs a protected app-server as well as a protected TUI. Each
host owns its backend for the lifetime of the session, including while no viewer
is attached. Native sessions keep the configured `codex.listen`. On enforced
platforms hosted sessions warn when they bypass that shared endpoint. Existing
shared hosts remain running and advisory until restore, and the explicit
`[codex] hosted_app_server = "shared"` opt-out keeps them advisory. Doctor,
status and session list surface that mode. Restore must verify that the hosted
backend can load the existing conversation before claiming migration support.
Measure backend memory per hosted session and verify approvals, delivery,
timelines and Observe against the dedicated connection.

## Operations that need the app

Creating an Observe grant with `lines` scope (either policy) and rebind require
an explicit macOS app action. A grant can durably disclose message text, including
future messages under automatic publication. Rebind can transfer the identity
and delivery authority of a session. These two classes are the initial list.
Permission answers, desk decisions, archive, metadata grants, list and revoke
use the routine path under the accepted lineage limit.

The CLI submits a bounded, immutable request and waits for the app. The app
shows the exact action, target and scope. A short-lived request is atomically
consumed before dispatch and canceled on requester disconnect. Only that
connection receives the result. The app's existing action supplies the gesture
for a request initiated in the app. An approval is valid for one stored request.
Ignore all client assertions of consent, and remove `auto_confirmed` from the
Observe contract. Fail with an app-availability error on macOS without the app,
and on Linux, WSL, and headless installations. An independently enrolled
hardware signer is future work.

Authenticate the app action on a per-message XPC endpoint and check the UID.
For each team allowed by the installation, require
`anchor apple generic and identifier "dev.senate.app" and certificate leaf[subject.OU] = "TEAM_ID"`.
Pin the team outside request input. This accepts a local Apple Development build
for a pinned team. Ad-hoc builds use isolated test infrastructure. The app pins
the daemon's signed service identity as well. The daemon needs signed distribution
and a launchd Mach service. XNU resolves `LOCAL_PEERTOKEN` from the peer socket's
last PID when queried. That identifies a process lookup and provides no
per-message code attestation. Keep unproved Unix actions on the request-submission
path.

## Hook and session authority

Authenticate hooks before state, paths, host bindings or session context can
change. Take the runtime PID only from verified peer ancestry and bind it to a
kernel process birth identity. Process names and raw PIDs are hints. Use a
kernel-authenticated message sender where available. A transport without a
stable peer identity needs a scoped launch credential for privileged sessions.
Keep old stored PIDs ineligible as authority until securely verified.

Bind a hosted session to the daemon's launch record and owner process. Preserve
a live registration only for the same verified tree. Reject claims to a person's
name or another session's seat. Names from env or a worktree can be aliases for
ordinary use. They confer no chair, secretary, or Speaker authority. A native
privileged seat switching process identities requires explicit person
reauthorization in the app or from a terminal that passes the routine guard.
Resolve session selectors once, check the same binding at use, and apply it to
role operations and lifecycle requests as well as hooks. A session ID alone
confers no authority.

Derive Git context from a verified process working directory or daemon-owned
launch state. Accept Observe transcripts only inside the runtime's expected
transcript tree, with regular-file checks and native IDs matched in the records.
A hook's delivery route and path hints carry no authority over another session.

## Design ablation

- Drop lineage and reparenting removes the only current rejection signal. A
  protected fork label retains the original lineage. Dropping migration control
  on Linux or clearing labels when a host exits loses that protection.
- Drop the app proof and the two sensitive operations inherit the routine
  path's native-agent and scheduler limitations. Dropping exact request binding
  or atomic consumption lets one gesture authorize a different or repeated act.
- Drop verified process ownership or role reauthorization and choosing another
  session ID or seat name can transfer its authority. Transcript-directory
  validation and matching record IDs address independent provenance checks.

References include [XNU coalitions](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/coalitions.md),
[XNU Unix socket implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c),
[Apple XPC per-message requirements](https://developer.apple.com/documentation/xpc/xpc_connection_set_peer_lightweight_code_requirement(_:_:)),
and [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html).

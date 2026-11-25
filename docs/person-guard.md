# Local IPC caller authorization

A Unix peer UID identifies an account. A coding agent running under that
account can change its name, terminal, and process parent. Senate checks hosted lineage and operator origin, requires local user
authentication for sensitive CLI requests, and binds session claims to a
verified process. A signed, pinned desktop app can submit its own explicit
UI actions. Trusted files, protected Keychain
items, trusted UI control, debugger access, and administrator access are outside
this threat model.

## Routine commands

Keep the routine CLI flow for Ghostty, Terminal, iTerm, tmux, SSH, and the
terminal attach viewer for routine operations. The viewer sends input to the host. Commands executed
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
  absent, hosting continues with advisory ancestry checks. Startup, status,
  and doctor surface that mode. `senate setup guard` explicitly installs the
  root-owned `/usr/local/libexec/senate-host-guard` helper and creates
  `/sys/fs/cgroup/senate-hosted`. The launcher enters the per-host scope and
  drops elevated credentials before executing the host. The daemon never
  installs it.

Lineage covers processes born in the hosted scope. An independent system
scheduler or a person-owned server can start a process in its own scope. On
macOS, operator receipts additionally require a known signed terminal,
verified system SSH process, or the pinned Senate app. This removes generic
launch jobs from the eligible set. An eligible terminal or multiplexer may
still act on input from another process. Native agents and shared app servers
are also advisory at this layer. The signed user-presence path is the control
for the sensitive IPC operations in the table. User-writable transcript files
retain their filesystem access policy. Routine IPC reads, including timelines,
history, search, and committee threads, still use the operator proof.

| Policy target | Hosted lineage | Routine caller checks | Sensitive requests | Hooks |
| --- | --- | --- | --- | --- |
| macOS GUI login | Inherited across reparenting | Signed origin plus ancestry. Person-owned deputies remain possible. | LocalAuthentication or explicit action by the pinned app | Verified process binding and native role reauthorization |
| macOS SSH or headless | Inherited across reparenting | Known system SSH remains eligible for reads | Refused without an interactive local path | Verified process binding and native role reauthorization |
| Linux/WSL with protected scope | Inherited inside cgroup | Scoped process handle plus advisory ancestry | Refused | Privileged claims require a verified process |
| Linux/WSL without protected scope | Advisory | Advisory ancestry | Refused | Privileged claims require a verified process |

The macOS bridge binds each operator proof to the exact request digest and
Mach sender's process generation for five seconds and consumes it once.
Managed launchd hosting persists its coalition before publishing the bridge.
Existing shared Codex backends remain advisory until restored to dedicated mode.

From an operator terminal in this worktree, with the signed fixture binaries
built from the same revision, run:

```sh
python3 deploy/check-person-terminal.py target/person-guard-scope-fixture
```

Expect `PASS: person terminal accepted; isolated grant list is empty`.
The check uses a fresh home and loopback port, and unloads the disposable
launchd job on exit.

Build the launcher as a normal user with
`sh deploy/build-host-guard.sh target/host-guard`. An administrator explicitly
installs the trusted artifact with `sudo senate setup guard --helper <path>
--sha256 <trusted-digest>`. The kernel must provide cgroup v2 and stable
Unix peer PID handles. Restart the unprivileged daemon to pick up the
provisioning.

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
canonical project directory using the same private `CODEX_HOME`. An isolated supervisor and backend used 68.7 MiB combined idle RSS. The
backend survived closure of its publication pipe and removed both sockets
on host-lease closure. Hosted restore checks that the new backend can load
the existing thread before publishing it.

## Sensitive requests

IPC permission answers, desk decisions, Observe grant creation and revocation,
line publication and withdrawal, native-seat authorization, rebind, chair,
secretary, role-alias and seat changes, committee archive and unarchive,
remote-machine administration and pairing, card writes, and operator-only
host stop, restore, pause, resume and replacement require a local decision.
History deletion and opening a writable terminal viewer also require local
authorization. Routine CLI speech uses the operator proof without a presence
prompt. Requests naming a session use its verified runtime instance.

New terminal hosts admit daemon control commands by process instance and
check one-use viewer tickets with a five-second lifetime. The ticket binds
the viewer's process generation and the read or write mode. The daemon
issues read-only tickets after verifying the operator. A locally authenticated
writable viewer can reuse the authorization within the same controlling
TTY and live session-leader instance for at most 12 hours. Missing kernel
identity or a background requester disables reuse. Programs sharing that same
TTY share the scope, including automation in an already authorized pane.
Detaching from tmux can leave the pane, its kernel TTY, and its authorization
alive. An authorized interactive viewer can still type directly into a
terminal prompt.
Existing pre-ticket hosts keep their old sockets until they restart. The
installer, session list, app, status and doctor identify that condition.
Versions that never recorded their version are displayed as unrecorded.
Attach can still open a legacy terminal to save work and prints a warning
before connecting.

Signed macOS requires the installed daemon pin for a replacement serving
the same home's bridge socket. Unsigned and unprovisioned Linux or WSL
also check the UID, active bridge peer and daemon executable path. This
is advisory. Provisioned Linux additionally verifies both connections
with pidfds and rejects a daemon in a hosted cgroup. Doctor probes a host
with an unverified daemon binding and reports any that still do not
recognize the current process.

Review the migration with
`senate session restart --all-outdated --dry-run`, then run
`senate session restart --all-outdated` or name one session at a time.
Restart requests a checkpoint, stops the terminal, and resumes the
recorded seat. It refuses recent interactive use or an active turn known
to the daemon unless forced. When Codex turn state cannot be verified it
also refuses without force. Pause timeout can leave no new checkpoint.
Save drafts and finish active work first.

Restart replaces the TUI and PTY. For Claude, Codex and agy it loses unsent
input, on-screen state and pending dialogs, and an in-flight turn does
not continue automatically. Saved runtime conversations remain the
restore target. Claude without a saved transcript starts empty under its
original ID. Dedicated Codex returns an error if it cannot load the
recorded thread. It never opens a substitute thread.

These viewer checks cover the terminal host's control, input and screen
sockets. For dedicated Codex sessions, the supervisor owns the Unix RPC
socket and checks the daemon and TUI process instances. The app-server
listens on authenticated loopback WebSocket. Its bearer stays in the
supervisor, and the app-server receives only the bearer digest. Dedicated
mode requires a Codex build with authenticated WebSocket support.

The CLI waits while the pinned app shows the request and requester derived
from kernel process data. LocalAuthentication uses a fresh context with the
system's device-owner policy, which can use Touch ID, Apple Watch, or the
login password. When the app is unavailable, the daemon launches its own
pinned executable in a dedicated helper mode and sends the details over a
private pipe. The helper verifies device-owner presence and reports success
only to its parent. The queue binds consent to one immutable request and the
same live IPC connection. Disconnect, timeout, cancellation and authentication
failure abort the request. App buttons that initiate a sensitive action use
the app's pinned identity and require no additional authentication prompt.
Client confirmation flags carry no authority.

The system mediates biometric and password authentication. An ordinary
Accessibility click carries no biometric evidence and transfers no OS consent
result to Senate. A tool with UI automation rights can still drive ordinary
app buttons. A terminal emulator with Accessibility or Automation rights
can also carry those grants to tools run through it. Check the terminal
emulators and automation grants in System Settings under Privacy & Security.
Doctor checks only whether macOS trusts the invoking process for Accessibility.
It can also warn about indexed team-signed copies with a different code hash.
Compare the requester's PID and TTY in the authentication sheet to the
terminal you used. The app cannot independently identify the foreground tab.
Control of the trusted app, usable login credentials, and a person accepting
a misleading request are outside this guarantee.

Install or upgrade from the operator's terminal with
`senate setup service install --yes --app /path/to/Senate.app`. The installer
records the app and daemon code-directory hashes under the trusted Senate
home. The daemon binary doubles as the signed helper. Every app reply checks
its audit token against the recorded app hash and the requirement
`anchor apple generic and identifier "dev.senate.app" and certificate leaf[subject.OU] = "TEAM_ID"`.
The helper uses `dev.senate.daemon` and the same Apple-issued team requirement.
The installation pin controls which binary can act when development and
release builds share a signing key. Apple Development and Developer ID both
work with the configured team. Developer ID and notarization are a future
release option. This branch does not establish usability of any particular
private signing key by a hosted agent. Doctor reports a missing, changed, or
inactive app pin. Unsigned development appliances have no trusted presence
path.

## The machine server's credentials

The machine's person and native Senate seats use verified local bridge IPC.
Their machine-server requests go through the private in-process router.
The public loopback router has no person bearer for the production machine.
Upgrade revokes the old `senates/top/token` credential and removes the file.
An old copy gets 401 directing its client to the local Senate CLI or app.
Doctor warns if the file is still present. Standalone servers and separately
configured parent connections keep their configured credentials.

No machine Senate seat has a general-purpose server bearer saved on disk.
Native Claude inbox credentials can be recorded with the session in
`state.db`. They authenticate delivery into that runtime's inbox, so a
same-user reader can potentially inject input there. They cannot authorize
server event writes or substitute for another runtime's verified IPC identity.
External desk intake uses [separate scoped worker credentials](worker-credentials.md).

An agent that can drive a user's running tmux or screen pane can act with
the operator proof available in that pane. It can speak through routine
person IPC and share writable viewer authorization already cached for the
pane. A detached pane can remain alive. Attachment and foreground process
group facts do not reliably identify the user's focused pane across
multiplexers, nor distinguish synthetic input in the same pane. Restrict
access to terminal automation and check which terminal tools have it.

Committee logs, including thread, memory, search and brief data on disk,
are protected by the filesystem like transcripts. IPC verification restricts
the daemon endpoints and does not provide a separate filesystem boundary.

## Writable trust records

The app pin and approval preference live in the Senate home. The installer
also records the accepted values in the daemon's SQLite state. A mismatch is
reported at startup, in status, in doctor, and in the app. The next sensitive
request asks for device-owner authentication before accepting a matching
team-signed app change. If the pin file has disappeared, confirmation can
restore the last accepted values. The current daemon must match the pin
before accepting a change. The Settings option to require Touch ID for
approvals is off by default. Turning it off through the app requires fresh
device-owner authentication.

A same-user process able to rewrite both records can make them agree.
This check detects divergence, including a preference reset in just one
record. It provides no independent protection against coordinated changes to
both records. A root-owned record would provide a stronger boundary. Senate
keeps both records in the user home to allow automatic installs and updates.

XNU's Unix peer token resolves the socket's last PID when queried. Per-message
Mach attestation checks the process generation and app identity.

## Session claims

Bind hook and role actions to the runtime's verified process identity, including
its generation. Reserve person names and reject another live session's seat.
Treat self-chosen names as ordinary aliases. Chair, secretary, and Speaker
authority requires a daemon-controlled launch or a person-approved binding. A
native privileged seat moving to a different process requires reauthorization.
Resolve a selector once and carry that proof through dispatch. Native Git
metadata still comes from the hook report. It provides context and carries no
role authority. Observe requires runtime-owned transcript paths and matching
native record IDs.

Native role commands use a session proof distinct from operator proof. A
different process cannot refresh a live binding. A privileged native
seat moving to a new verified process waits for operator authorization.
Run `senate session authorize <native-session-id>` from a person terminal.
It shows the executable, signing team, working directory, hosted status and
start time. Type `yes` to request system authentication for that candidate.
The daemon matches the runtime against its configured executable path, consumes
the proposal once, and rechecks the process before committing it.

## Ablation

- Without a protected lifetime label, reparenting removes the ancestry signal.
  Writable migration controls or clearing labels on host exit defeat it again.
- Remove the signed-origin check and separate launch jobs can qualify for
  routine receipts. Remove OS authentication and any eligible terminal acting
  for another process can request sensitive powers. Remove the code pin and an
  alternative binary signed with the same key can respond to the queue. Remove
  exact one-time binding and an approval can authorize a different action.
- Without process-generation ownership, session IDs and aliases become usable
  as credentials. Path and record checks protect independent transcript inputs.

Platform references include [XNU coalitions](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/coalitions.md),
[XNU Unix socket credentials](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c),
[Apple XPC per-message requirements](https://developer.apple.com/documentation/xpc/xpc_connection_set_peer_lightweight_code_requirement(_:_:)),
and [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html).

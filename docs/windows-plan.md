# Windows support: survey and plan

## Update, 2026-11-27: native re-baseline at main fbee6529

Machine: Windows 11 Pro 10.0.26100.7623, x64, over SSH into native
`cmd.exe` (not WSL; WSL is installed there but has no distribution, so
nothing below is WSL coverage). Rust 1.100.0 `x86_64-pc-windows-msvc`, VS
Community 2026 18.12 (cl.exe), Windows SDK 10.0.26100. No Node, Claude
Code, Codex or agy on that machine, so no real-runtime probe was possible
(open questions 1–3 below are still open). The WSL results are in
docs/wsl-test.md, from a different machine.

Since the survey, main gained three crates that were Unix-only, and
dea7496 never reached main. At fbee6529, `cargo check --workspace
--all-targets --keep-going` fails before the bridge:

| Crate | Errors | Cause |
| --- | --- | --- |
| senate-scene (lib) | 8 | `rustix` openat/`O_NOFOLLOW`, `nlink`, `DirBuilderExt` |
| senate-remote-admin (lib) | 28 | root-owned store: uid/mode checks, `O_NOFOLLOW`, sudo, `fchown`, `tcflush`, `umask` |
| senate-observe (lib) | 2 | `OpenOptionsExt::mode`; because senate-server depends on it, the server no longer builds on Windows either |
| pty-process (dependency of the bridge) | 65 | Unix-only crate |
| senate-work, test `docker_runner` | 1 | `make_executable` is `cfg(unix)` |

The first commit on `feat/windows-support` fixes those without changing
Unix behaviour: observe sets 0600 only on Unix; scene's folder checks
(`Bundle`, `read`, `pack`) are Unix-only, while manifest and archive rules
build everywhere; remote-admin keeps its Unix checks in one `os` module,
whose Windows arm fails closed (no root-owned store, no sudo, so no remote
device is ever authorized); the bridge's `libc`, `pty-process` and
`rustix` are Unix-only dependencies; and the tests that fake programs with
`#!/bin/sh` scripts are Unix-only. Afterwards every crate below the
bridge checks clean on Windows, and their tests pass there (the counts
are in the commit's report on Senate thread 88).

### The bridge on Windows

`cargo check` then reaches senate-bridge: 147 errors in the lib, about
110 of them in hosted terminals (`host.rs` 37, `host/control.rs` 34,
`host/scratch.rs` 10, `host/terminal.rs` 9, `host/paste.rs` 8,
`host/codex_backend.rs` 5, `host/watch.rs` 3, `host/view.rs` 2,
`daemon/hosts.rs` 6). The rest, by concern:

- **IPC** (`tokio::net::Unix*`): `ipc.rs`, `daemon.rs`, `daemon/{activity,
  session_drop,session_timeline}.rs`, `delivery{,/acp,/inbox,/codex}.rs`,
  `remote/terminal.rs`, tests in `cli.rs` and `mcp.rs`.
- **Caller identity**: `daemon.rs` takes the peer's uid and pid from
  `peer_cred`; `daemon/approval_guard.rs` uses `libc` and calls
  `operator_only`, which exists only on macOS and Linux;
  `kernel_process.rs` has no Windows arm.
- **Files**: modes and `rustix` in `safe_path.rs`, `checkpoint.rs`,
  `git_read.rs`, `fd.rs`, `follow.rs` (`flock`), `paths.rs`, `init.rs`,
  `install.rs`, `state.rs`, `remote{,/audit}.rs`, `doctor.rs`,
  `cli_observe_record.rs`, `daemon/session_history.rs`,
  `hook/diagnostics.rs`.
- **Processes and the service**: `session.rs` (12, `ps`/`kill`),
  `service.rs` (8), `hook.rs` and `mcp.rs` (`parent_id`), the daemon's
  SIGTERM.
- **Scenes**: `cli_scene.rs` (Bundle and pack are Unix-only now).

### What changed in the proposal: the IPC transport

The survey proposed loopback TCP and a token, on the grounds that no code
used the peer's credentials. That is no longer true. Since the survey,
the daemon has authorized callers by the socket's peer: it reads
`peer_cred` (uid and pid), walks the pid's ancestry to refuse
person-only requests from inside an agent (`refuse_agent_caller`), binds
session claims to a verified process (`BoundProcess`), and on macOS
checks each signed proof against that pid (docs/person-guard.md). A
loopback TCP connection carries no peer pid, and a token file shows only
that the caller can read a 0600 file, which every agent the person runs
can do. TCP and a token would therefore erase the person/agent boundary,
so I no longer propose it.

On Windows, the transport where the kernel reports the peer is a **named
pipe**. `GetNamedPipeClientProcessId` gives the client's pid, the pipe's
DACL admits only the person's SID (the 0600 equivalent), and tokio
supports it natively (`tokio::net::windows::named_pipe`). AF_UNIX on
Windows reports a peer pid (`SIO_AF_UNIX_GETPEERPID`), but tokio and mio
have no AF_UNIX there, so it would be our own socket code. Proposal: Unix
sockets stay on Unix and named pipes serve Windows, with the same line
protocol. The split is one seam (bind, accept, connect, peer pid), dictated
by the OS. This replaces open question 4.

### Caller identity on Windows

docs/person-guard.md has a row for Linux without a protected scope:
advisory ancestry, and Speaker-level requests refused. Windows fits that
row. It needs a Windows arm of `kernel_process` (boot identity; birth =
process creation time from `GetProcessTimes`; parent pid from a Toolhelp
snapshot, with a parent's creation time later than its child's treated
as pid reuse, because Windows never reparents), and `operator_only` /
`protected_operator_class` defined there as on Linux without a scope.
Until that exists, the code fails closed: `inspect` returns Unsupported,
so `refuse_agent_caller` refuses every person-only request and no session
can bind. Windows ancestry is weaker than Unix ancestry: any process can
name another process of the same user as its parent
(`PROC_THREAD_ATTRIBUTE_PARENT_PROCESS`), so ancestry stays advisory. A
Job Object would play the role of the Linux cgroup scope for hosted
sessions. It belongs to the hosted phase.

### Next slice

1. Bridge compiles on Windows: hosted terminals report "not supported on
   Windows yet" (one gate); IPC on a named pipe behind the seam above;
   the `kernel_process` Windows arm; files, processes and the service as
   proposed below.
2. Evidence: on Windows, `senated --machine` and `senate` with an isolated
   `SENATE_HOME` against a local mock Senate server: status, join, MCP
   tools over stdio, hooks, and the person-only refusals.
3. Hosted sessions on ConPTY stay a separate phase (section 2 below).

---

Status: investigation only, nothing ported. Branch `explore/windows`, from
main 1fabefc. Scope: the developer machine (`senate`, `senated --machine`,
senate-bridge, senate-app-core) on Windows 10/11 with Claude Code and Codex
for Windows, plus a check that the server side (`senated --config`) builds.

## Summary

- The server crates are already portable. senate-core, senate-log
  (SQLite and Postgres), senate-server, senate-speaker and senate-work build
  with MSVC and pass their tests on Windows 11. The few failures are in the
  tests, not the code.
- `senated` does not build on Windows, in either mode, because it links
  senate-bridge, and its chat-gateway IPC uses Unix sockets too.
- senate-bridge is where the work is. Every concern below is in it: local
  IPC over Unix sockets (nine places), hosted terminals on a Unix PTY with
  process groups and signals, launchd and systemd only, Unix file modes and
  `flock`, `ps` and `kill(pid, 0)`, and hook commands in POSIX-shell syntax.
- Proposed order: (1) changes with no visible effect, which leave Unix
  behaviour as it is; (2) the IPC on one transport for every platform,
  plus a Windows service, which gives a useful Windows machine: the CLI,
  the daemon, MCP, hooks and the Speaker, without hosted sessions; (3)
  hosted sessions on ConPTY.

## Compile and test results (Windows 11 build 26200, Rust 1.100 MSVC)

Source: `git archive` of 1fabefc, then the one Cargo.toml change below.

| Command | Result |
| --- | --- |
| `cargo check --workspace --all-targets` (as on main) | stops in the `pty-process` dependency (65 errors: it is Unix-only). The C dependencies (`libsqlite3-sys` bundled, `ring`, `zstd-sys`) build fine with VS 2022. |
| the same, with `libc`, `pty-process` and `rustix` moved under `[target.'cfg(unix)'.dependencies]` in senate-bridge (commit dea7496) | senate-bridge lib: 81 errors (92 with its tests). senated and senate-app-core are not checked, because they depend on it. Everything else checks clean except one test target (below). |
| `cargo test -p senate-core -p senate-log -p senate-server -p senate-speaker` | all pass except 1: `senate-speaker tests::secretary::show_names_the_source_of_each_model_key_and_hides_the_key`. The test uses `$HOME` as the "secret" key value. On Windows `HOME=C:\Users\Yuanliao`, which is a prefix of every temp path shown, so the check that the secret is not printed fails. This is a test bug: use a dedicated env var with a unique value. |
| `cargo test -p senate-work` | lib: 41 pass. `example_config`: pass. `gh_forge`: 4 of 6 fail, because the fake `gh` is a `#!/bin/sh` script (os error 193, "not a valid Win32 application"). `docker_runner` does not compile: `make_executable` is `#[cfg(unix)]` but is called unconditionally. Both are test-only. |

Errors in the bridge lib, by file (counted on Windows, the first error
only per use): host.rs 25, host/terminal.rs 11, daemon.rs 7,
daemon/hosts.rs 6, service.rs 5, session.rs 4, host/watch.rs 4, follow.rs
4, ipc.rs 3, then 1–2 each in state, mcp, install, init, host/control,
hook/diagnostics, delivery/{inbox,acp,codex,agy}, delivery, paths,
host_approval, host/view, hook. By kind: `rustix` 29, `std::os::unix` 20,
`tokio::net::unix*` / `UnixStream` / `UnixListener` 21, `libc` 7,
`pty_process` 6, `tokio::signal::unix` 5, `std::os::fd` 2. These are
resolution errors. Type errors will follow once they are fixed, but they
should be in the same places.

Other facts checked on the machine:

- `std::fs::File::{lock, try_lock, try_lock_shared}` work on Windows, and
  the locks are **mandatory**. While one handle holds the exclusive lock,
  another handle's `read_to_string` fails with OS error 33 ("another process
  has locked a portion of the file"). This matters for the hosts' `owner`
  file, see below.
- `std::os::windows::net::UnixListener` exists but is still unstable
  (`windows_unix_domain_sockets`, rust#150487). tokio has no AF_UNIX on
  Windows at all.
- Codex 0.164 on Windows accepts `--remote unix://PATH` and ws://, and has
  `codex app-server proxy` and `app-server daemon`. Senate's default Codex
  transport, `Transport::Proxy`, talks to Codex over the stdio of
  `codex app-server proxy`, so it needs no socket of ours.
- Codex is installed by npm as `codex.cmd` (and `codex`, a sh shim). Claude
  Code is `claude.exe` (2.1.228) under `%USERPROFILE%\.local\bin`, with
  `claude.cmd` from npm also on PATH.
- The Claude Code binary contains `CLAUDE_CODE_MESSAGING_SOCKET`,
  `udsInboxShape`, `CLAUDE_CODE_GIT_BASH_PATH` and `\\.\pipe\` strings.
  From the binary alone I could not tell what transport its inbox uses on
  Windows (see open questions).

## Survey by concern

Sizes: S is under 50 changed lines, M is 50–300, L is over 300.

### 1. Local IPC (Unix domain sockets)

| Socket | Where | Size |
| --- | --- | --- |
| daemon `bridge.sock` (CLI, MCP server, hooks): listen, accept, "is a daemon already there" probe, 0600 mode | `daemon.rs:142,589–691,5290–5360`; client `ipc.rs:238,1795` | M |
| per-host `tty` (attach), `ctl` (stop), `see` (watchers) | `host.rs:28,472–501,1067–1122`, `host/control.rs`, `host/watch.rs`, `host/terminal.rs:317`, `daemon/hosts.rs:1185` | M |
| ACP delivery socket (private, to the ACP shim) | `delivery/acp.rs:18,74,367,389` | S |
| Claude Code's inbox, `CLAUDE_CODE_MESSAGING_SOCKET` (theirs; we are the client) | `delivery/inbox.rs:34,121` | S, but depends on Claude Code |
| Codex control socket, `Transport::Unix` (theirs) | `delivery/codex.rs:249` | S |
| macOS app link | `senate-app-core/src/link.rs:25,64,97` | S |
| chat-gateway adapter IPC (server side, `senated --config`) | `senated/src/gateway_ipc/transport.rs:24,276`, `transport/socket.rs` | S–M |
| socket path limit (`SOCKET_PATH_LIMIT`, `SocketPathTooLong`) | `paths.rs:24–26,147` | removed by the proposal |

**Proposal: loopback TCP and a token, on every platform.**
`tokio::net::TcpListener` bound to `127.0.0.1:0`. The daemon writes its
port to `~/.senate/bridge.addr`. Every connection starts with a line that
carries a secret from `~/.senate/ipc.token`, a 0600 file. Each host gets its
own port file and uses the same token. This is the one IPC primitive that
tokio supports the same way everywhere. Named pipes on Windows beside
AF_UNIX on Unix would be two transports, and AF_UNIX on Windows has no
async support in tokio (mio dropped it). Security stays the same: today,
access means being able to open a 0600 socket. Afterwards it means being
able to read a 0600 token file (on Windows, a file under the user's
profile, which no other user can read). Since no code uses peer
credentials (no `peer_cred`/`ucred` anywhere), nothing else is lost. The
path-length limit and its error disappear. Binding to loopback does not
trigger a Windows Firewall prompt. The change is mostly mechanical: tokio's
`TcpStream` has the same `into_split`/`OwnedReadHalf` shape. The handshake
needs one new frame in the IPC contract (`ipc.rs`). The two foreign sockets
(Claude Code's inbox and Codex's control socket) are not ours to choose:
Codex already has the portable proxy path, and Claude Code's is open
question 1.

### 2. Hosted terminals

| Item | Where | Size |
| --- | --- | --- |
| PTY: `pty_process::open`, `Command::spawn(pts)`, resize, async read/write | `host.rs:24,482–501,744–800,76–93` | L |
| detaching the host: `setsid` | `host.rs:466` | S |
| the daemon's publication pipe on fd 3 (`tokio::net::unix::pipe`) | `host.rs:457–460` | S |
| stopping the TUI: `kill_process_group` TERM then KILL, the macOS EPERM case | `host.rs:847–880,1004–1056`; `daemon/hosts.rs:142–145` | M |
| signals in the host and viewers: TERM, HUP, INT, WINCH | `host.rs:30,470`; `host/terminal.rs:328–331`; `host/view.rs:662–665` | S each |
| attach client: termios raw mode, `tcgetpgrp`, `FIONREAD`, nonblocking `AsyncFd` stdin/stdout | `host/terminal.rs:11–15,185–301` | M |
| hosted-approval clock: `rustix::time::clock_gettime` | `host_approval.rs:10` | S |
| executable check: `rustix::fs::access(EXEC_OK)` | `host.rs:360`, `daemon/hosts.rs:744` | S |

**Proposal.**

- PTY: **`portable-pty`** (wezterm) on every platform, replacing
  `pty-process`. Its one API runs on openpty and on ConPTY (Windows 10 1809
  or later). Its reader and writer are blocking, so each runs on a
  `spawn_blocking` thread that feeds the tokio side through a channel. The
  `vt100` screen and the colour answers stay as they are.
- Stopping, gracefully: close the PTY master on every platform. Unix sends
  SIGHUP to the TUI's foreground group, and ConPTY sends CTRL_CLOSE_EVENT
  to its console clients. A forced kill after the grace period is the
  **one `cfg` split** I think is truly needed: the OS primitive that holds
  a process tree differs. On Unix it is `killpg` (as today). On Windows it
  is a Job Object (`windows-sys`, `AssignProcessToJobObject` at spawn, then
  `TerminateJobObject`), about 30 lines on each side. A sweep of
  descendants by parent pid would work on both, but it misses grandchildren
  that daemonize. MCP servers started by the TUIs are the common case, so I
  would not rely on it.
- Detaching the host: `setsid` on Unix and `creation_flags(DETACHED_PROCESS
  | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)` on Windows. This
  is a few lines in `host::start`. Spawn flags differ by OS, so there is no
  shared primitive.
- Publication pipe: read the host's plain stdin on a blocking thread on
  every platform, instead of the fd-3 dup.
- Window size: **poll `terminal_size`** (already a dependency) every
  ~200 ms on every platform, instead of SIGWINCH. Windows has no SIGWINCH,
  and the console only reports resizes as input records, which are lost in
  VT input mode.
- Attach client raw mode: `crossterm::terminal::enable_raw_mode` on every
  platform. On Windows, also set `ENABLE_VIRTUAL_TERMINAL_INPUT` on stdin
  (a few lines of `windows-sys`), so that a plain read yields VT bytes to
  forward unchanged. Stdin and stdout move to blocking threads instead of
  `AsyncFd`. The `tcgetpgrp` foreground check stays Unix-only, because
  Windows has no background jobs.
- Ending the host and viewer: TERM, HUP and INT become `tokio::signal::ctrl_c`
  plus the stop request on the control channel. A Windows console app only
  gets ctrl-c, ctrl-break and close.
- Clock: `std::time::Instant`/`SystemTime`. Executable check: the `which`
  crate (below).

### 3. The service

`service.rs` (63 hits) writes a launchd plist or a systemd user unit, and
runs `launchctl`/`systemctl`. `cli.rs:1525` refuses any other OS.

**Proposal: a per-user Task Scheduler logon task**, as a third `Platform`
arm. Install with `schtasks /Create /TN Senate\senated /XML <file>`, using a
LogonTrigger, `RestartOnFailure` and no `ExecutionTimeLimit`. Use
`/Run`, `/End`, `/Query /XML` and `/Delete` for the other steps. The XML is
built like the plist, and its tests are written the same way. A Windows
Service is the wrong fit: it runs in session 0, outside the user's logon,
so it can neither use the user's Claude Code and Codex logins nor host
their TUIs. A `HKCU\...\Run` key cannot restart the daemon. Size: M.
`/End` kills the daemon with TerminateProcess. A graceful stop therefore needs a new
IPC request that asks the daemon to shut down. None exists today. It would
work the same on Unix.

### 4. Paths, permissions, locks

| Item | Where | Size |
| --- | --- | --- |
| 0700 dirs / 0600 files (`DirBuilderExt`, `OpenOptionsExt`, `PermissionsExt`) | `paths.rs:18,162`, `host.rs:188,346–351,417–424`, `init.rs:189,198`, `install.rs:38,784,824`, `state.rs:49`, `daemon.rs:125`, `senated/src/machine.rs:67,463,658`, `gateway_ipc/transport/socket.rs` | M (many small) |
| `flock`: host `owner` lock, branch-file lock, hook diagnostics | `host.rs:25,194–303`, `follow.rs:337–355`, `hook/diagnostics.rs:7,107` | S–M |
| home layout `~/.senate` (`dirs::home_dir`) | `paths.rs` | none: `%USERPROFILE%\.senate` works |

**Proposal.**

- One pair of helpers, `paths::private_dir` and `paths::private_file`. On
  Unix they set the modes. On Windows they rely on the profile's
  owner-only ACL. That is a small `cfg` inside two functions, and every
  call site goes through them. The mode checks in tests (`mode() & 0o777`)
  become Unix-only.
- **`std::fs::File` locks** (stable since 1.89, within rust-version 1.95)
  replace `flock` everywhere: one mechanism, and verified on the machine.
  Because Windows locks are mandatory, the host must stop writing the
  TUI's pid into the locked `owner` file (`host.rs:494–496`). Today
  `child_pid`, `kill_all` and `terminate` read it while it is locked, and
  on Windows those reads fail. Write the pid to its own file (`tui-pid`)
  instead, on every platform.
- `remove_dir_all` on a host directory: a file that is still open cannot
  be removed on Windows. The host's own cleanup runs after its handles are
  closed, which is fine. `prune` removes another host's directory while it
  holds the `owner` handle open (`host.rs:254–265`), so it must close the
  lock handle first, or rename the directory and then remove it.

### 5. Shell and program-resolution assumptions

| Item | Where | Size |
| --- | --- | --- |
| hook commands written into Claude Code settings: `shell_quote(exe) hook <sub>` (POSIX quoting) | `install.rs:268–281` | S |
| hosted overlay: `… >/dev/null 2>&1 \|\| :` | `host.rs:768` | S |
| Codex hook commands, same quoting | `install/codex.rs:42–43` | S |
| `display_cli` prints `CLAUDE_CONFIG_DIR=… 'claude' …` | `install.rs:477–491` | S |
| PATH lookup with `mode & 0o111` | `install.rs:174–181`, `service::locate_senated` | S |
| sibling `senated` lookup (no `.exe`) | `service.rs:389` | S |
| `ps -o ppid=` for the ancestor chain | `session.rs:488` | S |
| `kill(pid, 0)` liveness, `parent_id()` | `session.rs:504–514`, `hook.rs:358`, `mcp.rs:1022`, `hook/diagnostics.rs:64` | S |
| notifications: `osascript` / `notify-send` | `notify.rs` | S |
| `git -c core.hooksPath=/dev/null` | `senate-work/src/git.rs:124` | none: Git for Windows maps `/dev/null` |

**Proposal.**

- Hook commands: drop the shell redirection and `|| :`. Instead, the
  `senate hook hosted-approval` subcommand itself writes nothing and always
  exits 0. Then the command is just `<exe> hook <sub>`, with the exe quoted
  only when its path has a space. Which quoting depends on the shell Claude
  Code and Codex use for hooks on Windows (open question 2). Today
  `install.rs` writes POSIX quoting. If Claude Code runs hooks in Git Bash,
  that stays right, and only the `/dev/null` part was a problem.
- Program lookup: the **`which`** crate on every platform. It handles
  `PATHEXT`, `.exe` and `.cmd`. Name siblings with
  `std::env::consts::EXE_SUFFIX`. `senated` next to `senate` becomes
  `senated.exe`, with no other change.
- Process facts: the **`sysinfo`** crate (refreshing only the pids asked
  for) on every platform. It replaces `ps`, `kill(pid, 0)` and
  `parent_id()`, and is one mechanism instead of three.
- Notifications: `notify-rust` would cover macOS, Linux and Windows
  toasts, but it is a new dependency. Phase 3, or skip notifications on
  Windows at first.
- Codex as `codex.cmd`: CreateProcess (and so ConPTY) cannot start a
  `.cmd`. A hosted Codex needs the real `codex.exe`, which the npm package
  ships under `node_modules/@openai/codex/…`. Running it through
  `cmd /c` risks the batch-file argument-escaping hazard. Open question 3.

### 6. Anything else

- Tests fake programs with `#!/bin/sh` scripts: senate-work `gh_forge` and
  `docker_runner`; in the bridge, `delivery/agy.rs`, `service.rs` tests,
  `tests/{hosted,hosted_approvals,companion,cli,mcp}.rs` and
  `tests/support/common/fake_codex.rs`; in senate-e2e, `continuity.rs` and
  its `fake_codex.rs`. Proposal: one fake program written in Rust (a `[[bin]]`
  in senate-e2e or a test-support crate, configured by env vars), used on
  every platform.
- Tests use `UnixStream::pair()` (`host/watch.rs`, `host.rs:1332`,
  `gateway_ipc/transport/tests.rs`). These move with the IPC change.
- `std::os::unix::fs::symlink` in tests (`install.rs:935`, `state.rs:2789`)
  stays Unix-only. Windows needs privileges or developer mode for
  symlinks.
- No inotify, kqueue or FSEvents anywhere. Directory watching is a 1 s
  poll (`host.rs` `vanished`), which is portable.
- `tokio::signal::unix` in `senated/src/assembly.rs:666` is already
  `cfg`-split correctly.
- senate-app-core is only the macOS app's core. On Windows it needs only
  the IPC change in order to compile. No Windows app is in scope.

## Phased plan

**Phase 0: changes with no visible effect** (each lands on main without
changing Unix behaviour; add a Windows CI job that runs `cargo check
--workspace` and the tests of the crates that build):
std file locks and the separate `tui-pid` file; the `private_dir` and
`private_file` helpers; `which` and `EXE_SUFFIX`; `sysinfo` for process
facts; portable test fakes; the speaker test's `HOME` bug; the
`docker_runner` `cfg`. About M in total.

**Phase 1: a useful Windows machine without hosted sessions.** IPC on
loopback TCP and a token, everywhere (daemon, MCP, hooks, CLI, ACP, app
link, chat gateway); daemon shutdown on ctrl-c and on a new IPC shutdown request; the Task
Scheduler service; hook commands without shell syntax. Hosted-session
commands report "not supported on Windows yet": that is the one `cfg` gate
until phase 2. The result: `senate setup`, `senate install` for Claude
Code and Codex, the MCP tools, hooks, inbox, the Speaker, catch-up, and
Codex delivery through the proxy, all on Windows. `senated --config` also
builds and runs there. Claude Code inbox delivery depends on open
question 1. Size: L, mostly mechanical.

**Phase 2: hosted sessions.** `portable-pty` everywhere; the attach client
on crossterm raw mode, VT input and polled size; the Windows detach flags
and Job Object kill; the hosted Claude approval overlay; the `codex.exe`
resolution. Size: L. This phase carries most of the risk.

**Phase 3: polish.** Notifications, `senate doctor` checks for Windows,
docs and install instructions, and a release build for
`x86_64-pc-windows-msvc`.

## Risks

- **ConPTY is not a byte pipe.** conhost re-renders the screen and answers
  some queries itself (DA, DSR). It may drop or reorder the OSC 10/11
  colour queries that the host answers at startup (`host.rs` pump). The
  hosted TUIs could look wrong, or wait on a query, until this is
  handled. Test early in phase 2.
- **Task Scheduler and child processes.** Hosted terminals must outlive a
  daemon restart. It needs checking whether `schtasks /End` or a restart
  after failure kills the task's whole process tree, and whether
  `CREATE_BREAKAWAY_FROM_JOB` is allowed from inside a scheduled task's
  job.
- **The IPC change touches every platform.** Moving macOS and Linux off
  Unix sockets keeps one mechanism, but it is a change to the hot path for
  everyone. A token check on every connection, and the stale `bridge.addr`
  case (a port reused by another program), need tests.
- **Mandatory locks on Windows** can surface elsewhere: any file read
  while another process holds a lock on it.
- **Antivirus and SmartScreen** may slow or block a freshly built
  `senate.exe` that spawns processes and opens ports. This is likely on
  managed machines.

## Open questions for Wendell

1. **Claude Code's inbox on Windows.** What does Claude Code put in
   `CLAUDE_CODE_MESSAGING_SOCKET` on Windows: a named pipe (`\\.\pipe\…`),
   an AF_UNIX path, or nothing? This decides whether push delivery to
   non-hosted Claude sessions works on Windows. I can check it in one
   minute with a SessionStart hook that dumps the environment, but it runs
   `claude -p` once on the CI machine's Claude account. May I?
2. **Which shell runs hooks** for Claude Code and Codex on Windows: Git
   Bash, cmd or PowerShell? I can check this the same way, at the same
   cost.
3. **Hosted Codex from npm.** Is it acceptable to resolve
   `codex.cmd` to the `codex.exe` inside the npm package, or should
   Windows require a standalone `codex.exe` install?
4. **One IPC transport for every platform.** Loopback TCP and a token
   (proposed) changes macOS and Linux too. The alternative is to keep Unix
   sockets on Unix and add named pipes on Windows: two transports, against
   the single-mechanism rule, but no change for existing installs. Which
   do you want?
5. **Windows targets.** Windows 10 1809 or later (needed for ConPTY) and
   x86_64 only, or also ARM64 Windows?

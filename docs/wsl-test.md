# WSL test on a Windows 11 test machine (2026-11-21)

Machine: Windows 11 build 26200, WSL 2.3.24, kernel 5.15.153.1, distro
`Ubuntu` 24.04 (systemd=true was already set). Senate main 1fabefc, then
this branch's setup changes (754c727).

## 1. Senate in WSL

| Step | Result |
| --- | --- |
| apt build-essential, pkg-config; rustup (1.100); nvm Node 26; Claude Code 2.1.330 (native installer); Codex 0.171 (npm) | ok |
| `cargo build --release -p senated -p senate-bridge`, `cargo install` both | ok, 1m18s |
| `claude -p "say ok"` through the gateway (copied `env` from the Windows settings.json) | ok |
| `codex exec "say ok"` | **fails** with "Reconnecting… waiting for network". Cause found later: Codex resolves names itself (strace shows its own A and AAAA queries to WSL's DNS tunnel), and the company DNS answers `AAAA <gateway>` with REFUSED. That holds for both WSL's DNS tunnel and the LAN DNS server. Codex then never connects. glibc (curl, Node) ignores the refusal. With the gateway pinned in /etc/hosts (tested in a private mount namespace, the distro's file unchanged) `codex exec` answers `ok`. The workaround is in the README. Hosted Codex: see below. |
| `senate setup init --local --person <user>`, `senate setup install --yes` | ok; hooks and MCP use /home/ubuntu/.cargo/bin/senate |
| `senate setup service install --yes` + `loginctl enable-linger ubuntu` | ok; unit active, agents recorded as /home/ubuntu/.local/bin/claude and the nvm codex |
| `senate join wsl` in a repo on the WSL filesystem | ok |
| `senate spawn claude --alias hosted` | ok; the TUI first shows Claude Code's onboarding (theme, security notes, folder trust), answered through `senate attach` |
| `senate ask wsl::hosted "17×3" --wait` | answered `51` |
| plain Claude Code in a tmux terminal in a second worktree: joined on first prompt; `senate ask <full id> "6×7" --wait` | answered `42` (inbox delivery) |
| the plain session asks `wsl::hosted` with `mcp__senate__ask`, answer pushed back | ok (`19`) |
| `senate session list`, `senate session watch` (grid), `senate attach` + Ctrl-] | ok |
| daemon restart (`setup service install --yes` again) with a hosted session running | the hosted session kept running (15 min uptime after) |

Hosted Codex, with the gateway pinned temporarily in the distro's
/etc/hosts (one line, `/etc/wsl.conf` unchanged, removed afterwards; the
file is byte-identical to before):

| Step | Result |
| --- | --- |
| `senate spawn codex --alias cx` before the Codex app-server daemon runs | refused: "cannot reach the Codex app-server (… app-server proxy): closed before the WebSocket upgrade" |
| `codex app-server daemon start` (as `senate setup install` says), then `senate spawn codex --alias cx` | ok |
| `senate attach wsl::cx`: Codex asks to trust the folder, then to review the 3 new senate hooks; "Trust all and continue" | ok; the session joins `wsl` |
| `senate ask wsl::cx "8×9" --wait` | answered `72` |
| `senate session list` | `wsl::cx … codex … running` |
| `senate session stop wsl::cx --force`, then `senate session restore wsl::cx` | stopped, then running again under the same id |
| attach after the restore | the earlier turn (`72`) is on screen |
| `senate ask wsl::cx "what did I ask before?" --wait` | "You asked what 8 × 9 is; I answered 72." |

The Codex app-server daemon is not a systemd unit: `codex app-server
daemon start` runs it as a plain background process (`backend: pid`). It
stops with the distro, and must be started again (or managed with
`codex app-server daemon bootstrap`) before a hosted Codex can start.

What broke:

- Before 754c727, the Windows PATH that WSL appends could have Senate pick
  Windows programs: with nvm only in `~/.bashrc`, a login shell's `codex`
  was `/mnt/c/Users/…/npm/codex` (a wrapper that fails: `node: not
  found`), and `claude` without the native install was the Windows npm
  `claude`. The service's PATH also carried every `/mnt/c` directory. Fixed
  in 754c727 (setup ignores `/mnt/<drive>` directories in WSL).
- `notify-send` is missing in WSL, so desktop notifications fail (logged
  once). Not fixed; a Windows toast from WSL would need `powershell.exe`.
- Not WSL-specific: `senate ask claude-7ddea704 …` (a prefix of a senator
  id) was accepted and posted to a senator that does not exist, so it
  waited out its 120 s. The full id worked. Worth a separate fix: reject or
  resolve an unknown senator name.

## 2. Keeping the daemon alive

- Docker Desktop's WSL integration keeps `wsl.exe -d Ubuntu …` processes
  attached at all times, so on the test machine the distro never stops. With every
  terminal of ours closed it stayed Running for over 3 minutes, and it has
  been up since 2026-11-12.
- The keepalive task (README, "Windows (WSL)") registered as
  `Senate WSL keepalive` for the Windows user. `Start-ScheduledTask` started
  `conhost --headless wsl.exe -d Ubuntu --exec sleep infinity`, and a
  `sleep infinity` appeared in the distro. `schtasks /End` ended only
  conhost: the `wsl.exe` under it and the `sleep` kept running until the
  `sleep` was killed. The task is now disabled.
- Not tested yet, waiting for approval: `wsl --terminate Ubuntu`, to show
  the daemon comes back by itself on the distro's next start (linger), and
  the task bringing the distro up. A logoff/logon was ruled out on this
  machine (other services run there).

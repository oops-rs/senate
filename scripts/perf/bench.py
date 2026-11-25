#!/usr/bin/env python3
"""A reproducible Senate machine to measure, and its headline numbers.

    cargo build --profile perf -p senated -p senate-bridge --bins
    scripts/perf/bench.py setup          # an isolated machine under /tmp/senate-perf
    scripts/perf/bench.py run            # print the numbers (--json FILE keeps them)
    scripts/perf/bench.py stop           # end what setup started, and only that

`setup` makes a Senate home of its own (HOME and SENATE_HOME under the
root, a free loopback port, nothing of the calling session's environment),
starts `senated --machine` there and fills it: committees, hosted sessions
whose TUI is a small shell script, past sessions, and a log of notes and
questions. Every process it starts is recorded by pid under the root, and
`stop` ends those pids and no others.

The daemon must be the `perf` build (optimised, debug assertions on): an
unsigned daemon takes a person's requests only as a debug fixture confined
to a temp home. Numbers are therefore a little above a signed release's on
the paths that carry debug assertions, and exclude the signed build's
caller attestation. Compare a change with the same build before and after.

Python 3.9+, standard library only. macOS and Linux.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import fcntl
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import termios
import time
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
DEFAULT_ROOT = Path("/tmp/senate-perf")

# What a `senate` or `senated` started here must never inherit
# (crates/senate-bridge/src/testenv.rs), and the markers of a coding session.
INHERITED = [
    "SENATE_TEST_STRICT_IDENTITY",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "CODEX_THREAD_ID",
    "SENATE_HOME",
    "SENATE_AGENT",
    "SENATE_VERBOSE",
    "ANTIGRAVITY_CONVERSATION_ID",
    "SENATE_HOSTED_CLAUDE",
    "SENATED_CONFIG",
    "RUST_LOG",
    "SENATE_CONFINE_TO",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
]

FAKE_TUI = r"""#!/bin/sh
# A coding TUI for the benchmark: it reports itself to Senate as Claude Code
# does (the session-start hook, with a transcript), paints one line and
# then waits. In a directory named `flood` it first prints FLOOD_MB of text
# as fast as the terminal takes it; in one named `busy` it repaints a
# status line ten times a second, as a working agent's spinner does.
id=""
for a in "$@"; do
    case "$a" in
        --session-id=*) id="${a#--session-id=}" ;;
        --resume=*) id="${a#--resume=}" ;;
    esac
done
transcript="$HOME/.claude/projects/bench/$id.jsonl"
if [ -n "$id" ] && [ ! -f "$transcript" ]; then
    mkdir -p "$HOME/.claude/projects/bench"
    i=0
    while [ $i -lt __TURNS__ ]; do
        printf '{"type":"user","sessionId":"%s","uuid":"u%s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"%s","message":{"role":"user","content":"Question %s: where is the retry limit set, and what changes if it moves into the gateway configuration?"}}\n' "$id" "$i" "$PWD" "$i"
        printf '{"type":"assistant","sessionId":"%s","uuid":"a%s","timestamp":"2026-01-01T00:00:01.000Z","cwd":"%s","message":{"role":"assistant","content":[{"type":"text","text":"Answer %s: the limit is read once at start from the gateway section. Moving it changes three call sites and one test. The retry loop itself stays as it is, and the backoff keeps its ceiling of sixty seconds."}]}}\n' "$id" "$i" "$PWD" "$i"
        i=$((i + 1))
    done > "$transcript"
fi
if [ -n "$id" ]; then
    printf '{"session_id":"%s","cwd":"%s","transcript_path":"%s","hook_event_name":"SessionStart","source":"startup"}' \
        "$id" "$PWD" "$transcript" | CLAUDE_CODE_SESSION_ID="$id" CLAUDE_PID=$$ "__SENATE__" hook session-start >/dev/null 2>&1
fi
if [ -n "$id" ]; then
    # Hooks the benchmark asks for run here, below this process, as Claude
    # Code's do: the daemon takes a session's hook only from its own tree.
    fifo="$PWD/.bench-$id.fifo"
    rm -f "$fifo"
    mkfifo "$fifo"
    ( exec perl "__HELPER__" "$fifo" "$id" "$PWD" "$transcript" "__SENATE__" $$ ) >/dev/null 2>&1 &
fi
printf 'fixture TUI %s\n' "$id"
case "$PWD" in
    */flood)
        bytes=$(cat "$PWD/flood.bytes")
        perl -MTime::HiRes=time -e 'printf "%.6f\n", time' > "$PWD/flood.start"
        yes 'the quick brown fox jumps over the lazy dog 0123456789 the quick brown fox jumps' | head -c "$bytes"
        perl -MTime::HiRes=time -e 'printf "%.6f\n", time' > "$PWD/flood.end.tmp"
        mv "$PWD/flood.end.tmp" "$PWD/flood.end"
        ;;
    */busy)
        (
            n=0
            while :; do
                printf '\r\033[2Kworking %s' "$n"
                n=$((n + 1))
                sleep 0.1
            done
        ) &
        ;;
esac
while IFS= read -r line; do
    case "$line" in
        paint) printf 'fixture TUI\n' ;;
        q) exit 0 ;;
    esac
done
"""

HOOK_HELPER = r"""#!/usr/bin/perl
# Runs a session's hooks on request, inside the session's process tree.
# Lines on the FIFO: `hook <count> <result file>` times that many
# user-prompt-submit hooks; `wait <marker> <result file>` runs the hook
# until its output carries the marker and writes when that was.
use strict;
use warnings;
use Time::HiRes qw(time sleep);
my ($fifo, $id, $cwd, $transcript, $senate, $tui) = @ARGV;
$ENV{CLAUDE_CODE_SESSION_ID} = $id;
$ENV{CLAUDE_PID} = $tui;
my $payload = "$fifo.payload";
open(my $p, ">", $payload) or die;
print $p qq({"session_id":"$id","cwd":"$cwd","transcript_path":"$transcript","hook_event_name":"UserPromptSubmit","prompt":"x"});
close $p;
sub hook { return scalar `"$senate" hook user-prompt-submit < "$payload" 2>/dev/null`; }
sub put { my ($file, $text) = @_; open(my $o, ">", "$file.tmp") or return; print $o $text; close $o; rename "$file.tmp", $file; }
while (1) {
    open(my $in, "<", $fifo) or last;
    while (my $line = <$in>) {
        chomp $line;
        my ($cmd, $arg, $file) = split / /, $line, 3;
        if ($cmd eq "hook") {
            my @ms;
            for (1 .. $arg) { my $t = time; hook(); push @ms, sprintf("%.3f", (time - $t) * 1000); }
            put($file, join("\n", @ms) . "\n");
        } elsif ($cmd eq "wait") {
            my $deadline = time + 15;
            my $found = 0;
            while (time < $deadline) {
                if (index(hook(), $arg) >= 0) { $found = time; last; }
                sleep 0.02;
            }
            put($file, sprintf("%.6f\n", $found));
        }
    }
    close $in;
}
"""


class Fixture:
    def __init__(self, root: Path, bin_dir: Path):
        self.root = root
        self.home = root / "h"
        self.senate_home = self.home / ".senate"
        self.work = root / "w"
        self.pids = root / "pids"
        self.senate = bin_dir / "senate"
        self.senated = bin_dir / "senated"
        self.tui = root / "fixture-claude"
        self.helper = root / "fixture-helper.pl"

    def env(self, **extra: str) -> dict[str, str]:
        env = {k: v for k, v in os.environ.items() if k not in INHERITED}
        env["HOME"] = str(self.home)
        env["SENATE_HOME"] = str(self.senate_home)
        env["SENATE_CONFINE_TO"] = str(self.root)
        env.update(extra)
        return env

    def cli(self, *args: str, cwd: Path | None = None, stdin: str | None = None,
            check: bool = True, **extra: str) -> subprocess.CompletedProcess:
        done = subprocess.run(
            [str(self.senate), *args],
            env=self.env(**extra),
            cwd=str(cwd or self.root),
            input=stdin,
            capture_output=True,
            text=True,
        )
        if check and done.returncode != 0:
            raise SystemExit(f"senate {' '.join(args)}: {done.stderr.strip() or done.stdout.strip()}")
        return done

    def daemon_pid(self) -> int | None:
        return read_pid(self.pids / "daemon")

    def record(self, name: str, pid: int) -> None:
        self.pids.mkdir(parents=True, exist_ok=True)
        (self.pids / name).write_text(f"{pid}\n")

    def host_pids(self) -> list[int]:
        """The terminal hosts of this fixture: `senate session-host` processes
        whose working directory argument is under the root."""
        out = subprocess.run(["ps", "-Ao", "pid=,command="], capture_output=True, text=True).stdout
        pids = []
        for line in out.splitlines():
            pid, _, command = line.strip().partition(" ")
            if " session-host " in command and str(self.work) in command.replace("/private", ""):
                pids.append(int(pid))
        return pids

    def tui_pids(self) -> list[int]:
        out = subprocess.run(["ps", "-Ao", "pid=,command="], capture_output=True, text=True).stdout
        return [
            int(line.strip().split(" ", 1)[0])
            for line in out.splitlines()
            if str(self.tui) in line.replace("/private", "") and "session-host" not in line
        ]


def read_pid(path: Path) -> int | None:
    try:
        pid = int(path.read_text().strip())
    except (OSError, ValueError):
        return None
    try:
        os.kill(pid, 0)
    except OSError:
        return None
    return pid


def listening(path: Path) -> bool:
    with socket.socket(socket.AF_UNIX) as s:
        try:
            s.connect(str(path))
        except OSError:
            return False
    return True


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def git_init(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q", str(path)], check=True, capture_output=True)


# ---- setup -------------------------------------------------------------------


def setup(fx: Fixture, args: argparse.Namespace) -> None:
    if fx.root.exists():
        raise SystemExit(f"{fx.root} exists: run `bench.py stop --remove` first, or pick another --root")
    for binary in (fx.senate, fx.senated):
        if not binary.exists():
            raise SystemExit(
                f"{binary} is missing: cargo build --profile perf -p senated -p senate-bridge --bins"
            )
    fx.home.mkdir(parents=True)
    fx.work.mkdir()
    fx.cli("setup", "init", "--local", "--person", "bench")
    config = fx.senate_home / "senate.toml"
    text = re.sub(r'(?m)^listen = ".*"$', f'listen = "127.0.0.1:{free_port()}"', config.read_text())
    text += f'\n[companion]\nclaude_bin = "{fx.tui}"\n'
    config.write_text(text)
    fx.helper.write_text(HOOK_HELPER)
    fx.tui.write_text(
        FAKE_TUI.replace("__SENATE__", str(fx.senate))
        .replace("__HELPER__", str(fx.helper))
        .replace("__TURNS__", str(args.turns))
    )
    fx.tui.chmod(0o700)

    start_daemon(fx)
    committees = [f"c{i:02d}" for i in range(1, args.committees + 1)]
    for c in committees:
        git_init(fx.work / c)
        fx.cli("join", c, cwd=fx.work / c)

    t0 = time.monotonic()
    aliases = []
    for i in range(args.sessions):
        c = committees[i % len(committees)]
        alias = f"s{i + 1:02d}"
        fx.cli("spawn", "claude", "--committee", c, "--alias", alias, "--dir", str(fx.work / c))
        aliases.append((c, alias))
    spawn_ms = (time.monotonic() - t0) * 1000 / max(1, args.sessions)
    print(f"spawned {args.sessions} hosted sessions ({spawn_ms:.0f} ms each)")

    for i in range(args.past):
        c = committees[i % len(committees)]
        alias = f"p{i + 1:03d}"
        fx.cli("spawn", "claude", "--committee", c, "--alias", alias, "--dir", str(fx.work / c))
        fx.cli("session", "stop", f"{c}::{alias}", check=False)
    print(f"spawned and stopped {args.past} past sessions")

    # The log: mostly notes, some questions to the seated agents (open
    # threads on the docket), spread over every committee.
    body = (
        "The retry limit is three now, and the backoff keeps its ceiling of sixty seconds. "
        "Nothing else in the gateway changed; the test that pinned the old value is updated."
    )

    def post(n: int) -> None:
        c = committees[n % len(committees)]
        if n % 10 == 0 and aliases:
            ac, alias = aliases[n % len(aliases)]
            fx.cli("ask", alias, f"Question {n}: {body}", "--committee", ac, check=False)
        else:
            fx.cli("tell", "*", f"Note {n}: {body}", "--committee", c, check=False)

    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        list(pool.map(post, range(args.events)))
    rate = args.events / max(1e-9, time.monotonic() - t0)
    print(f"posted {args.events} events ({rate:.0f}/s through the CLI, 8 at a time)")
    print(f"ready: {fx.root}  (daemon pid {fx.daemon_pid()})")


def start_daemon(fx: Fixture) -> int:
    log = open(fx.root / "daemon.log", "ab")
    child = subprocess.Popen(
        [str(fx.senated), "--machine"],
        env=fx.env(),
        cwd=str(fx.root),
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=log,
        start_new_session=True,
    )
    fx.record("daemon", child.pid)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise SystemExit(f"senated exited: see {fx.root / 'daemon.log'}")
        if listening(fx.senate_home / "bridge.sock"):
            return child.pid
        time.sleep(0.05)
    raise SystemExit("the daemon did not answer within 30 s")


# ---- stop --------------------------------------------------------------------


def stop(fx: Fixture, args: argparse.Namespace) -> None:
    if not fx.root.exists():
        print(f"{fx.root}: nothing there")
        return
    hosts = fx.host_pids()
    tuis = fx.tui_pids()
    daemon = fx.daemon_pid()
    for pid in [daemon, *hosts, *tuis]:
        if pid:
            try:
                os.kill(pid, signal.SIGTERM)
            except OSError:
                pass
    deadline = time.monotonic() + 10
    left = [p for p in [daemon, *hosts, *tuis] if p]
    while left and time.monotonic() < deadline:
        time.sleep(0.2)
        left = [p for p in left if alive(p)]
    for pid in left:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    print(f"stopped the daemon, {len(hosts)} hosts and {len(tuis)} TUIs")
    if args.remove:
        shutil.rmtree(fx.root, ignore_errors=True)
        print(f"removed {fx.root}")


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    # A zombie still answers signal 0.
    out = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout
    return bool(out.strip()) and not out.strip().startswith("Z")


# ---- measuring ---------------------------------------------------------------


def cpu_seconds(pid: int) -> float:
    """User plus system CPU time of a process, in seconds. Not that of the
    processes it starts (`git`, or a host's `defaults` where it still runs one): no
    tool reads a running process's reaped children from outside it."""
    if sys.platform != "darwin":
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        except OSError:
            return 0.0
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return 0.0
    days = 0
    if "-" in out:
        d, out = out.split("-", 1)
        days = int(d)
    parts = [float(p) for p in out.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0.0)
    return days * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]


def rss_mb(pid: int) -> float:
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out) / 1024 if out else 0.0


def threads(pid: int) -> int:
    if sys.platform == "darwin":
        out = subprocess.run(["ps", "-M", "-p", str(pid)], capture_output=True, text=True).stdout
        return max(0, len(out.splitlines()) - 1)
    return len(os.listdir(f"/proc/{pid}/task"))


def descriptors(pid: int) -> int:
    if sys.platform == "darwin":
        out = subprocess.run(["lsof", "-p", str(pid)], capture_output=True, text=True).stdout
        return sum(1 for line in out.splitlines()[1:] if re.match(r"\d", line.split()[3]))
    return len(os.listdir(f"/proc/{pid}/fd"))


def context_switches(pid: int) -> int | None:
    """Cumulative context switches (macOS `top`), or None where unknown."""
    if sys.platform != "darwin":
        try:
            text = Path(f"/proc/{pid}/status").read_text()
        except OSError:
            return None
        return sum(int(m) for m in re.findall(r"ctxt_switches:\s+(\d+)", text))
    out = subprocess.run(
        ["top", "-l", "1", "-pid", str(pid), "-stats", "pid,csw"], capture_output=True, text=True
    ).stdout
    for line in out.splitlines():
        parts = line.split()
        if parts and parts[0] == str(pid) and len(parts) > 1:
            return int(parts[1].rstrip("+-"))
    return None


def footprint_mb(pid: int) -> float | None:
    """Physical footprint (what Activity Monitor shows as Memory), macOS."""
    if sys.platform != "darwin":
        return None
    out = subprocess.run(
        ["top", "-l", "1", "-pid", str(pid), "-stats", "pid,mem"], capture_output=True, text=True
    ).stdout
    for line in out.splitlines():
        parts = line.split()
        if parts and parts[0] == str(pid) and len(parts) > 1:
            m = re.match(r"([\d.]+)([KMG])", parts[1])
            if m:
                return float(m.group(1)) * {"K": 1 / 1024, "M": 1, "G": 1024}[m.group(2)]
    return None


def idle(fx: Fixture, seconds: float) -> dict:
    daemon = fx.daemon_pid()
    if not daemon:
        raise SystemExit("the fixture daemon is not running: bench.py setup")
    hosts = fx.host_pids()
    before = {p: cpu_seconds(p) for p in [daemon, *hosts]}
    csw0 = context_switches(daemon)
    host_csw0 = [context_switches(p) for p in hosts[:3]]
    t0 = time.monotonic()
    time.sleep(seconds)
    elapsed = time.monotonic() - t0
    csw1 = context_switches(daemon)
    host_csw1 = [context_switches(p) for p in hosts[:3]]
    after = {p: cpu_seconds(p) for p in [daemon, *hosts]}

    def pct(pids: list[int]) -> float:
        return round(100 * sum(after[p] - before[p] for p in pids) / elapsed, 2)

    host_csw = [
        (b - a) / elapsed for a, b in zip(host_csw0, host_csw1) if a is not None and b is not None
    ]
    return {
        "idle_window_s": round(elapsed, 1),
        "daemon_cpu_pct": pct([daemon]),
        "daemon_ctx_switches_per_s": None if csw0 is None or csw1 is None else round((csw1 - csw0) / elapsed, 1),
        "daemon_rss_mb": round(rss_mb(daemon), 1),
        "daemon_footprint_mb": footprint_mb(daemon),
        "daemon_threads": threads(daemon),
        "daemon_descriptors": descriptors(daemon),
        "hosts": len(hosts),
        "hosts_cpu_pct_total": pct(hosts),
        "host_ctx_switches_per_s": round(statistics.mean(host_csw), 1) if host_csw else None,
        "host_rss_mb_mean": round(statistics.mean(rss_mb(p) for p in hosts), 1) if hosts else None,
        "host_footprint_mb": footprint_mb(hosts[0]) if hosts else None,
        "host_threads": threads(hosts[0]) if hosts else None,
    }


def timed(fx: Fixture, name: str, argv: list[str], runs: int, stdin: str | None = None,
          cwd: Path | None = None, **extra: str) -> dict:
    env = fx.env(**extra)
    samples = []
    failures = 0
    for _ in range(runs):
        t0 = time.perf_counter()
        done = subprocess.run(
            [str(fx.senate), *argv], env=env, cwd=str(cwd or fx.root), input=stdin,
            capture_output=True, text=True,
        )
        samples.append((time.perf_counter() - t0) * 1000)
        failures += done.returncode != 0
    samples.sort()
    return {
        "name": name,
        "median_ms": round(statistics.median(samples), 1),
        "p95_ms": round(samples[min(len(samples) - 1, int(len(samples) * 0.95))], 1),
        "min_ms": round(samples[0], 1),
        "failures": failures,
    }


def window(fd: int, rows: int, cols: int) -> None:
    """Give a pseudo-terminal a size, as a real terminal has."""
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def first_session(fx: Fixture) -> tuple[str, Path] | None:
    """A live hosted session's native id and working directory."""
    out = subprocess.run(["ps", "-Ao", "command="], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if " session-host " not in line or str(fx.work) not in line.replace("/private", ""):
            continue
        parts = line.split(" session-host ", 1)[1].split()
        # <host id> <runtime> <cwd> <native session id> ...
        if len(parts) >= 4 and not parts[2].endswith(("/flood", "/busy")):
            return parts[3], Path(parts[2])
    return None


def cli_latency(fx: Fixture, runs: int) -> list[dict]:
    rows = [
        timed(fx, "senate --version (process start)", ["--version"], runs),
        timed(fx, "senate status", ["status"], runs),
        timed(fx, "senate session list", ["session", "list"], runs),
        timed(fx, "senate committees", ["committees"], runs),
        timed(fx, "senate inbox", ["inbox", "--committee", "c01"], runs),
        timed(fx, "senate inbox --all", ["inbox", "--all", "--committee", "c01"], runs),
        timed(fx, "senate senator list", ["senator", "list"], runs),
        timed(fx, "senate session history", ["session", "history"], runs),
        timed(
            fx, "completion: senate attach <TAB>", ["--", "senate", "attach", ""], runs,
            SENATE_COMPLETE="zsh", _CLAP_COMPLETE_INDEX="2", _CLAP_IFS="\n",
        ),
    ]
    hook = hook_latency(fx, runs)
    if hook:
        rows.append(hook)
    return rows


def ask_helper(fx: Fixture, sid: str, cwd: Path, line: str, timeout: float) -> str | None:
    """Send one request to a session's hook helper and read its result."""
    fifo = cwd / f".bench-{sid}.fifo"
    result = fx.root / f"helper-{uuid.uuid4().hex[:8]}.out"
    if not fifo.exists():
        return None
    fd = os.open(str(fifo), os.O_WRONLY | os.O_NONBLOCK)
    try:
        os.write(fd, f"{line} {result}\n".encode())
    finally:
        os.close(fd)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if result.exists():
            text = result.read_text()
            result.unlink()
            return text
        time.sleep(0.01)
    return None


def hook_latency(fx: Fixture, runs: int) -> dict | None:
    """`senate hook user-prompt-submit` as a hosted session's own hook:
    what every turn of an agent waits for."""
    session = first_session(fx)
    if not session:
        return None
    sid, cwd = session
    # The first hooks take what is queued for the session; time the rest.
    ask_helper(fx, sid, cwd, "hook 3", 30)
    text = ask_helper(fx, sid, cwd, f"hook {runs}", 60)
    if not text:
        return {"name": "hook user-prompt-submit (hosted session)", "median_ms": None, "failures": runs}
    samples = sorted(float(x) for x in text.split())
    return {
        "name": "hook user-prompt-submit (hosted session)",
        "median_ms": round(statistics.median(samples), 1),
        "p95_ms": round(samples[min(len(samples) - 1, int(len(samples) * 0.95))], 1),
        "min_ms": round(samples[0], 1),
        "failures": 0,
    }


def post_latency(fx: Fixture, runs: int) -> dict:
    return timed(fx, "senate tell (one event appended)", ["tell", "*", "bench note", "--committee", "c01"], runs)


def attach_first_paint(fx: Fixture, runs: int) -> dict | None:
    """Time from starting `senate attach --read-only` to the TUI's screen."""
    session = first_session(fx)
    if not session:
        return None
    sid, _ = session
    samples = []
    for _ in range(runs):
        t0 = time.perf_counter()
        pid, fd = pty.fork()
        if pid == 0:
            env = fx.env(TERM="xterm-256color")
            os.chdir(fx.root)
            window(0, 40, 120)
            os.execve(str(fx.senate), [str(fx.senate), "attach", "--read-only", f"claude-{sid}"], env)
        seen = b""
        first = None
        answered = False
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            seen += chunk
            if not answered and b"\x1b[c" in seen:
                # Answer the terminal queries as a terminal would.
                os.write(fd, b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\x1b[?997;2n\x1b[?62;22c")
                answered = True
            if b"fixture TUI" in seen:
                first = (time.perf_counter() - t0) * 1000
                break
        try:
            os.write(fd, b"\x1d")
        except OSError:
            pass
        end = time.monotonic() + 2
        while time.monotonic() < end:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                break
            time.sleep(0.02)
        else:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
        os.close(fd)
        if first is not None:
            samples.append(first)
    if not samples:
        return {"name": "attach --read-only: first paint", "median_ms": None, "failures": runs}
    samples.sort()
    return {
        "name": "attach --read-only: first paint",
        "median_ms": round(statistics.median(samples), 1),
        "p95_ms": round(samples[-1], 1),
        "min_ms": round(samples[0], 1),
        "failures": runs - len(samples),
    }


def delivery(fx: Fixture, runs: int) -> dict | None:
    """From a question posted to a hosted session to the moment its next
    hook would carry it (the daemon has it queued for the session)."""
    session = first_session(fx)
    if not session:
        return None
    sid, cwd = session
    # Its primary name: the runtime and the native session id.
    alias = f"claude-{sid}"
    samples = []
    for _ in range(runs):
        marker = f"delivery-probe-{uuid.uuid4().hex[:8]}"
        t0 = time.time()
        fx.cli("ask", alias, marker, "--committee", cwd.name, check=False)
        text = ask_helper(fx, sid, cwd, f"wait {marker}", 20)
        found = float(text) if text else 0.0
        if found:
            samples.append((found - t0) * 1000)
    if not samples:
        return {"name": "ask -> queued for the session", "median_ms": None, "failures": runs}
    samples.sort()
    return {
        "name": "ask -> queued for the session",
        "median_ms": round(statistics.median(samples), 1),
        "p95_ms": round(samples[-1], 1),
        "min_ms": round(samples[0], 1),
        "failures": runs - len(samples),
    }


def flood(fx: Fixture, mb: int) -> dict | None:
    """Relay throughput: a hosted TUI that prints `mb` MiB as fast as its
    terminal takes it. The host's CPU time for it, and the wall time."""
    directory = fx.work / "flood"
    if directory.exists():
        shutil.rmtree(directory)
    git_init(directory)
    (directory / "flood.bytes").write_text(f"{mb * 1024 * 1024}\n")
    before = set(fx.host_pids())
    alias = f"flood{int(time.time()) % 100000}"
    fx.cli("spawn", "claude", "--committee", "c01", "--alias", alias, "--dir", str(directory), check=False)
    host = next(iter(set(fx.host_pids()) - before), None)
    deadline = time.monotonic() + 300
    while time.monotonic() < deadline and not (directory / "flood.end").exists():
        time.sleep(0.05)
    if not (directory / "flood.end").exists() or host is None:
        return None
    start = float((directory / "flood.start").read_text())
    end = float((directory / "flood.end").read_text())
    time.sleep(0.5)
    cpu = cpu_seconds(host)
    result = {
        "flood_mb": mb,
        "flood_wall_s": round(end - start, 2),
        "flood_mb_per_s": round(mb / max(1e-9, end - start), 1),
        "flood_host_cpu_s": round(cpu, 2),
        "flood_host_rss_mb": round(rss_mb(host), 1),
    }
    fx.cli("session", "stop", f"c01::{alias}", check=False)
    return result


def watch_grid(fx: Fixture, seconds: float) -> dict | None:
    """`senate session watch` over every hosted terminal: its CPU, and what
    it costs the daemon and the hosts while it is open."""
    daemon = fx.daemon_pid()
    hosts = fx.host_pids()
    if not daemon or not hosts:
        return None
    pid, fd = pty.fork()
    if pid == 0:
        env = fx.env(TERM="xterm-256color")
        os.chdir(fx.root)
        window(0, 60, 200)
        os.execve(str(fx.senate), [str(fx.senate), "session", "watch"], env)
    answered = False
    seen = 0
    end = time.monotonic() + 3
    while time.monotonic() < end:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            seen += len(chunk)
            if not answered and b"\x1b[c" in chunk:
                os.write(fd, b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\x1b[?997;2n\x1b[?62;22c")
                answered = True
    before = {p: cpu_seconds(p) for p in [pid, daemon, *hosts]}
    t0 = time.monotonic()
    painted = 0
    while time.monotonic() - t0 < seconds:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                painted += len(os.read(fd, 65536))
            except OSError:
                break
    elapsed = time.monotonic() - t0
    after = {p: cpu_seconds(p) for p in [pid, daemon, *hosts]}
    result = {
        "watch_tiles": len(hosts),
        "watch_cpu_pct": round(100 * (after[pid] - before[pid]) / elapsed, 2),
        "watch_daemon_cpu_pct": round(100 * (after[daemon] - before[daemon]) / elapsed, 2),
        "watch_hosts_cpu_pct_total": round(100 * sum(after[p] - before[p] for p in hosts) / elapsed, 2),
        "watch_bytes_painted_per_s": round(painted / elapsed),
        "watch_rss_mb": round(rss_mb(pid), 1),
        "watch_drew_anything": seen > 0,
    }
    try:
        os.write(fd, b"q")
    except OSError:
        pass
    time.sleep(0.3)
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass
    os.waitpid(pid, 0)
    os.close(fd)
    return result


def sizes(fx: Fixture) -> dict:
    out = {}
    for name, path in [
        ("state_db_mb", fx.senate_home / "state.db"),
        ("log_db_mb", fx.senate_home / "senates" / "top" / "log.db"),
    ]:
        # With its write-ahead log: a quiet daemon may not have checkpointed.
        total = sum(q.stat().st_size for q in (path, Path(f"{path}-wal")) if q.exists())
        if total:
            out[name] = round(total / 1048576, 2)
    out["senate_binary_mb"] = round(fx.senate.stat().st_size / 1048576, 1)
    out["senated_binary_mb"] = round(fx.senated.stat().st_size / 1048576, 1)
    return out


def run(fx: Fixture, args: argparse.Namespace) -> None:
    if not fx.daemon_pid():
        raise SystemExit("the fixture daemon is not running: bench.py setup")
    result: dict = {"root": str(fx.root), "build": str(fx.senate.parent)}
    only = set(args.only.split(",")) if args.only else None

    def want(part: str) -> bool:
        return only is None or part in only

    def part(name: str, label: str, measure) -> None:
        # One part failing leaves the others' numbers standing.
        if not want(name):
            return
        print(f"{label} …", file=sys.stderr)
        try:
            measure()
        except Exception as error:  # noqa: BLE001
            print(f"  {name}: {error!r}", file=sys.stderr)
        if args.json:
            Path(args.json).write_text(json.dumps(result, indent=2) + "\n")

    def cli_part() -> None:
        rows = cli_latency(fx, args.runs)
        rows.append(post_latency(fx, args.runs))
        first = attach_first_paint(fx, max(3, args.runs // 4))
        if first:
            rows.append(first)
        result["latency"] = rows

    def delivery_part() -> None:
        row = delivery(fx, 5)
        if row:
            result.setdefault("latency", []).append(row)

    part("idle", f"idle for {args.idle_secs:.0f} s", lambda: result.update(idle=idle(fx, args.idle_secs)))
    part("cli", "CLI latency", cli_part)
    part("delivery", "delivery", delivery_part)
    part("watch", "watch grid", lambda: result.update(watch=watch_grid(fx, 10)))
    part("flood", "relay throughput", lambda: result.update(flood=flood(fx, args.flood_mb)))
    result["sizes"] = sizes(fx)

    report(result)
    if args.json:
        Path(args.json).write_text(json.dumps(result, indent=2) + "\n")


def report(result: dict) -> None:
    def table(title: str, rows: list[tuple[str, object]]) -> None:
        print(f"\n{title}")
        width = max(len(k) for k, _ in rows)
        for key, value in rows:
            print(f"  {key.ljust(width)}  {value}")

    if "idle" in result:
        table("Idle (nothing happening)", list(result["idle"].items()))
    if "latency" in result:
        print("\nLatency (ms)")
        width = max(len(r["name"]) for r in result["latency"])
        print(f"  {'':{width}}  {'median':>8} {'p95':>8} {'min':>8}  failures")
        for r in result["latency"]:
            print(
                f"  {r['name'].ljust(width)}  {str(r.get('median_ms')):>8} {str(r.get('p95_ms')):>8} "
                f"{str(r.get('min_ms')):>8}  {r.get('failures', 0)}"
            )
    for key, title in [("watch", "Watch grid"), ("flood", "Relay throughput"), ("sizes", "Sizes")]:
        if result.get(key):
            table(title, list(result[key].items()))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT, help="the fixture's directory (short: sockets live under it)")
    parser.add_argument("--bin-dir", type=Path, default=REPO / "target" / "perf", help="where senate and senated are")
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("setup", help="make the fixture and start its daemon")
    s.add_argument("--committees", type=int, default=30)
    s.add_argument("--sessions", type=int, default=20)
    s.add_argument("--past", type=int, default=40, help="sessions started and stopped again")
    s.add_argument("--events", type=int, default=3000)
    s.add_argument("--turns", type=int, default=200, help="turns in each session's transcript")
    r = sub.add_parser("run", help="print the headline numbers")
    r.add_argument("--idle-secs", type=float, default=60)
    r.add_argument("--runs", type=int, default=20)
    r.add_argument("--flood-mb", type=int, default=20)
    r.add_argument("--only", help="comma-separated: idle,cli,delivery,watch,flood")
    r.add_argument("--json", help="also write the numbers to this file")
    t = sub.add_parser("stop", help="end the fixture's processes")
    t.add_argument("--remove", action="store_true", help="and delete the fixture")
    sub.add_parser("start", help="start the daemon of an existing fixture again")
    args = parser.parse_args()
    fx = Fixture(args.root, args.bin_dir)
    if args.command == "setup":
        setup(fx, args)
    elif args.command == "run":
        run(fx, args)
    elif args.command == "stop":
        stop(fx, args)
    elif args.command == "start":
        if fx.daemon_pid():
            raise SystemExit("the daemon is already running")
        print(f"daemon pid {start_daemon(fx)}")


if __name__ == "__main__":
    main()

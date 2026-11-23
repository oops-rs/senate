"""macOS hosted PTY latency ablation (Python 3.11+). Run with a matching senate/senated pair.

python3 crates/senate-bridge/examples/session_latency.py --bin-dir ~/.cargo/bin

A temporary LaunchAgent serves a private local Senate. The host runs a tiny
raw-mode echo application. Measure at the attach and watch clients' output
PTYs (before terminal-emulator rendering). Both clients see the same frames.
ABBA order, ten warmups per job, one nice +15 CPU burner per core by default.
"""
import argparse
import contextlib
import math
import os
from pathlib import Path
import plistlib
import pty
import re
import select
import subprocess
import sys
import tempfile
import termios
import time


@contextlib.contextmanager
def terminal(argv, root, env):
    master, slave = pty.openpty()
    try:
        termios.tcsetwinsize(slave, (32, 110))
        with subprocess.Popen(argv, cwd=root, env=env, stdin=slave, stdout=slave,
                              stderr=slave) as client:
            try:
                yield master
            finally:
                client.terminate()
    finally:
        os.close(slave)
        os.close(master)


def receive(fds, pattern):
    buffers = {fd: b"" for fd in fds}
    found = {}
    deadline = time.monotonic() + 10
    while buffers and time.monotonic() < deadline:
        ready, _, _ = select.select(buffers, [], [], max(0, deadline - time.monotonic()))
        for fd in ready:
            data = os.read(fd, 65536)
            received = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
            if not data:
                raise RuntimeError("terminal closed")
            buffers[fd] = buffers[fd][-2048:] + data
            match = pattern.search(buffers[fd])
            if match:
                found[fd] = (match, received)
                del buffers[fd]
    if buffers:
        raise RuntimeError(f"waiting for {pattern.pattern!r}, terminal tails "
                           f"{[buf[-250:] for buf in buffers.values()]!r}")
    return found


def samples(senate, root, env, count):
    with terminal([senate, "attach", "probe"], root, env) as attach, terminal(
        [senate, "session", "watch", "--all"], root, env
    ) as watch:
        receive([attach], re.compile(rb"\x1b\[c"))
        os.write(attach, b"\x1b[?62;22c")
        for i in range(count + 10):
            # Vary arrival phase relative to the grid's frame clock.
            time.sleep(0.005 + (i % 17) * 0.001)
            marker = re.compile(f"lat{i + 1}t(\\d+)end".encode())
            start = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
            os.write(attach, b"x")
            seen = receive([attach, watch], marker)
            frame, echoed = seen[attach]
            watched, painted = seen[watch]
            if frame[0] != watched[0]:
                raise RuntimeError("clients displayed different frames")
            emitted = int(frame[1])
            if not start <= emitted <= min(echoed, painted):
                raise RuntimeError("fixture and observer timestamps are inconsistent")
            if i >= 10:
                yield [(echoed - start) / 1e6, (echoed - emitted) / 1e6,
                       (painted - emitted) / 1e6]


def run(binaries, root, interactive, count):
    senate = binaries / "senate"
    label = f"dev.senate.latency.{root.name}"
    domain = f"gui/{os.getuid()}"
    env = dict(HOME=str(root), SENATE_HOME=str(root), SENATE_CONFINE_TO=str(root),
               PATH="/usr/bin:/bin", TERM="xterm-256color")
    fixture = root / "probe"
    fixture.write_text("#!/usr/bin/python3\n" + """
import ctypes, os, time, tty
from pathlib import Path
tty.setraw(0)
nice = os.getpriority(os.PRIO_PROCESS, 0)
qos = ctypes.CDLL(None).qos_class_self()
Path("ready.tmp").write_text(f"fixture nice={nice} main thread qos={qos:#x}")
os.rename("ready.tmp", "ready")
i = 0
while key := os.read(0, 1):
    if key != b"x":
        continue
    i += 1
    os.write(1, f"\\x1b[Hlat{i}t{time.clock_gettime_ns(time.CLOCK_MONOTONIC)}end\\x1b[K".encode())
""")
    fixture.chmod(0o700)
    (root / "senate.toml").write_text(
        f'person = "bench"\n[local]\nlisten = "127.0.0.1:0"\n'
        f'[codex]\nlisten = "unix://{root}/codex.sock"\n'
        f'[companion]\nclaude_bin = "{fixture}"\n')
    log = root / "daemon.log"
    job = dict(Label=label, RunAtLoad=True,
               ProgramArguments=[str(binaries / "senated"), "--machine"],
               EnvironmentVariables=env, StandardErrorPath=str(log))
    if interactive:
        job["ProcessType"] = "Interactive"
    plist = root / "job.plist"
    plist.write_bytes(plistlib.dumps(job))
    target = f"{domain}/{label}"
    subprocess.run(["launchctl", "bootstrap", domain, plist], check=True)
    try:
        for _ in range(200):
            if (root / "bridge.sock").is_socket():
                break
            time.sleep(0.05)
        else:
            raise RuntimeError(log.read_text())
        info = subprocess.check_output(["launchctl", "print", target], text=True)
        print(next(line.strip() for line in info.splitlines() if "spawn type =" in line),
              flush=True)
        subprocess.run([senate, "join", "perf"], cwd=root, env=env, check=True)
        subprocess.run([senate, "spawn", "claude", "--alias", "probe", "--committee",
                        "perf", "--dir", root], cwd=root, env=env, check=True)
        try:
            for _ in range(200):
                if (root / "ready").exists():
                    print((root / "ready").read_text(), flush=True)
                    break
                time.sleep(0.05)
            else:
                raise RuntimeError("hosted fixture did not enter raw mode")
            return list(samples(senate, root, env, count))
        finally:
            subprocess.run([senate, "session", "stop", "--all", "--force"], cwd=root,
                           env=env, check=True)
    finally:
        subprocess.run(["launchctl", "bootout", target], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--workers", type=int, default=os.cpu_count())
    args = parser.parse_args()
    if args.samples < 1 or args.workers < 0:
        parser.error("need positive samples and nonnegative workers")
    print(f"{args.workers} load workers, {args.samples} samples per job", flush=True)
    results = {False: [], True: []}
    with contextlib.ExitStack() as stack:
        for _ in range(args.workers):
            worker = stack.enter_context(subprocess.Popen(
                ["/usr/bin/nice", "-n", "15", sys.executable, "-c", "while True: pass"]))
            stack.callback(worker.terminate)
        for interactive in (False, True, True, False):
            with tempfile.TemporaryDirectory(prefix="lat-", dir="/tmp") as tmp:
                results[interactive].extend(run(args.bin_dir.resolve(), Path(tmp),
                                                interactive, args.samples))
    for mode, rows in results.items():
        for name, values in zip(("key/attach", "output/attach", "output/watch"), zip(*rows)):
            ordered = sorted(values)
            quantiles = [ordered[math.ceil(len(rows) * p) - 1] for p in (.5, .95, .99)]
            print(f"{'Interactive' if mode else 'default'} {name} n={len(rows)} "
                  "p50/p95/p99 ms " + "/".join(f"{v:.3f}" for v in quantiles))


if __name__ == "__main__":
    main()

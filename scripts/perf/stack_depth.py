#!/usr/bin/env python3
"""How much thread stack the daemon needs to start a hosted session.

    scripts/perf/stack_depth.py [--bin-dir target/perf]

Starts `senated --machine` in a throwaway home with smaller and smaller
thread stacks (RUST_MIN_STACK, which the tokio workers take) and asks each
for one hosted session, the deepest path found in the daemon. Prints the
smallest stack that still works and the largest that overflowed. The
default is 2 MiB. Uses the fixture of bench.py; leaves nothing running.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench  # noqa: E402


def survives(bin_dir: Path, stack_kib: int) -> bool:
    root = Path(tempfile.mkdtemp(prefix="senate-stack-", dir="/tmp"))
    fx = bench.Fixture(root, bin_dir)
    os.environ["RUST_MIN_STACK"] = str(stack_kib * 1024)
    try:
        fx.home.mkdir(parents=True)
        fx.work.mkdir()
        fx.cli("setup", "init", "--local", "--person", "bench")
        config = fx.senate_home / "senate.toml"
        text = config.read_text().replace(
            'listen = "127.0.0.1:7979"', f'listen = "127.0.0.1:{bench.free_port()}"'
        )
        config.write_text(text + f'\n[companion]\nclaude_bin = "{fx.tui}"\n')
        fx.helper.write_text(bench.HOOK_HELPER)
        fx.tui.write_text(
            bench.FAKE_TUI.replace("__SENATE__", str(fx.senate))
            .replace("__HELPER__", str(fx.helper))
            .replace("__TURNS__", "2")
        )
        fx.tui.chmod(0o700)
        try:
            bench.start_daemon(fx)
        except SystemExit:
            return False
        bench.git_init(fx.work / "c01")
        # The CLI and the host take the small stack too; only the daemon's
        # fate is read.
        fx.cli("join", "c01", cwd=fx.work / "c01", check=False)
        fx.cli("spawn", "claude", "--alias", "s01", cwd=fx.work / "c01", check=False)
        time.sleep(0.5)
        alive = fx.daemon_pid() is not None
        overflowed = "overflowed its stack" in (root / "daemon.log").read_text(errors="replace")
        return alive and not overflowed
    finally:
        os.environ.pop("RUST_MIN_STACK", None)

        class Quiet:
            remove = False

        try:
            bench.stop(fx, Quiet())
        finally:
            shutil.rmtree(root, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bin-dir", type=Path, default=bench.REPO / "target" / "perf")
    parser.add_argument("--sizes", default="2048,1024,512,256,192,128,96,64,48,32")
    args = parser.parse_args()
    works, fails = None, None
    for kib in [int(x) for x in args.sizes.split(",")]:
        ok = survives(args.bin_dir, kib)
        print(f"  {kib:5d} KiB: {'works' if ok else 'overflows or fails'}", flush=True)
        if ok:
            works = kib
        else:
            fails = kib
            break
    print(f"smallest stack that works: {works} KiB; largest that does not: {fails} KiB")


if __name__ == "__main__":
    main()

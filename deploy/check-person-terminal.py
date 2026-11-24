#!/usr/bin/env python3
"""Check person-only IPC from an operator terminal using disposable local state."""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time
import uuid


def check(binaries):
    daemon = (binaries / "senated").resolve(strict=True)
    client = (binaries / "senate").resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="senate-person-", dir="/tmp") as directory:
        home = Path(directory).resolve()
        (home / "senate.toml").write_text(
            'person = "fixture"\n[local]\nname = "person-terminal-fixture"\n'
            'listen = "127.0.0.1:0"\n'
        )
        digest = hashlib.sha256(os.fsencode(home)).hexdigest()[:24]
        confirmation = f"dev.senate.senated.person-approval.{digest}"
        label = f"dev.senate.person-terminal.fixture.{uuid.uuid4().hex}"
        domain = f"gui/{os.getuid()}"
        plist = home / "job.plist"
        plist.write_bytes(plistlib.dumps({
            "Label": label,
            "ProgramArguments": [str(daemon), "--machine", "--home", str(home)],
            "EnvironmentVariables": {"SENATE_HOME": str(home), "HOME": str(home)},
            "MachServices": {confirmation: True, f"{confirmation}.scope": True},
            "RunAtLoad": True,
            "StandardOutPath": str(home / "daemon.log"),
            "StandardErrorPath": str(home / "daemon.log"),
        }))
        subprocess.run(["/bin/launchctl", "bootstrap", domain, str(plist)], check=True, timeout=10)
        try:
            end = time.monotonic() + 10
            while not (home / "bridge.sock").exists():
                if time.monotonic() >= end:
                    raise RuntimeError("isolated daemon did not open its socket")
                time.sleep(0.05)
            result = subprocess.run(
                [str(client), "observe", "token", "list"],
                env=os.environ | {"SENATE_HOME": str(home)},
                capture_output=True, text=True, check=False, timeout=10,
            )
            if result.returncode or json.loads(result.stdout) != []:
                raise RuntimeError(result.stderr.strip() or "unexpected grant response")
            print("PASS: person terminal accepted; isolated grant list is empty")
        finally:
            subprocess.run(["/bin/launchctl", "bootout", f"{domain}/{label}"],
                           check=True, timeout=10)


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.platform != "darwin":
        sys.exit("Usage on macOS: python3 deploy/check-person-terminal.py SIGNED_BIN_DIR")
    check(Path(sys.argv[1]))

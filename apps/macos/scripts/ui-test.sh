#!/usr/bin/env bash
# Runs the app's tests: the unit tests (the store against a fake core), and
# the UI smoke test against the core's fake daemon, which this starts in a
# scratch Senate home and stops afterwards. Never touches ~/.senate.
#
# Usage, from apps/macos/: scripts/ui-test.sh [--unit-only]
set -euo pipefail

app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root="$(cd "$app_dir/../.." && pwd)"
cd "$app_dir"

xcodegen generate --quiet
only=()
if [[ "${1:-}" == "--unit-only" ]]; then
  only=(-only-testing:SenateTests)
fi

# The socket path must stay short (about 100 bytes).
home="$(mktemp -d /tmp/senate-ui.XXXXXX)"
chmod 700 "$home"
cargo build --quiet --manifest-path "$root/Cargo.toml" -p senate-app-core --example fake_daemon
# No approvals arriving mid-test: the lists hold still under the clicks.
SENATE_HOME="$home" FAKE_APPROVAL_EVERY=0 "$root/target/debug/examples/fake_daemon" >"$home/fake.log" 2>&1 &
fake=$!
trap 'kill $fake 2>/dev/null; rm -rf "$home"' EXIT
for _ in $(seq 50); do [[ -S "$home/bridge.sock" ]] && break; sleep 0.1; done

# TEST_RUNNER_ variables reach the test process without the prefix.
TEST_RUNNER_SENATE_UI_HOME="$home" xcodebuild test \
  -project Senate.xcodeproj -scheme Senate -destination 'platform=macOS' \
  -derivedDataPath build -resultBundlePath "build/tests-$(date +%s).xcresult" \
  ${only[@]+"${only[@]}"} 2>&1 \
  | grep -E "^(Test Suite|Test Case|.*error:|\*\* TEST|✔|✘|◇|􀢄|Testing|.*passed|.*failed)" || true

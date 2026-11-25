#!/bin/bash
# Staged acceptance check of a signed Senate release, run by the person from
# their own terminal BEFORE anything is installed live.
#
#   deploy/stage-check.sh <signed release dir> [0.12.3 senate binary]
#
# <signed release dir> holds the signed `senate` and `senated`
# (apps/macos/scripts/release.sh leaves them in build/release/). The
# optional second argument is an old `senate` (for example
# ~/.senate/bin/senate while 0.12.3 is installed): it is only executed, with
# the staged home, as the MCP client of a session from before the upgrade.
#
# It starts the signed daemon as a disposable launchd job with its own label
# and its own Senate home under /tmp, next to the live daemon, and never
# touches ~/.senate, the live job or live sessions. Hosted sessions in the
# staged home run a stand-in for Claude Code (a shell script), so no agent
# starts. Steps, each with PASS or FAIL and the reason:
#
#   1. a read-only operator request from this terminal
#   2. a sensitive request (spawning a chair) that asks you to authenticate
#   3. the hosted session that started uses its Senate MCP tools
#   4. that chair spawns a member through the MCP spawn tool
#   5. a session from before the upgrade (0.12.3 state, no caller proofs)
#      keeps its hook and MCP tools
#
# It cleans up by the exact launchd label and the recorded process IDs, and
# ends with one line: READY TO INSTALL, or NOT READY with the failing step.

set -u

die() { echo "stage-check: $*" >&2; exit 2; }

[[ "$(uname)" == Darwin ]] || die "macOS only"
[[ $# -ge 1 && $# -le 2 ]] || die "usage: deploy/stage-check.sh <signed release dir> [0.12.3 senate binary]"
rel="$(cd "$1" 2>/dev/null && pwd -P)" || die "no such directory: $1"
senate="$rel/senate"
senated="$rel/senated"
old="${2:-}"
[[ -x "$senate" && -x "$senated" ]] || die "$rel must hold senate and senated"
[[ -z "$old" || -x "$old" ]] || die "not an executable: $old"
repo="$(cd "$(dirname "$0")/.." && pwd -P)"
schema="$repo/crates/senate-bridge/tests/fixtures/state-v0.12.3.sql"
[[ -r "$schema" ]] || die "missing $schema"
command -v sqlite3 >/dev/null || die "sqlite3 is needed"

failed=""
pass() { echo "PASS  $1"; }
fail() {
  echo "FAIL  $1"
  echo "      $2" | head -c 2000
  echo
  [[ -n "$failed" ]] || failed="$1"
}

team="$(codesign -dv "$senated" 2>&1 | sed -n 's/^TeamIdentifier=//p')"
# SENATE_STAGE_UNSIGNED=1 runs the staging itself with an unsigned build,
# to check this script; such a run never says READY.
if [[ "${SENATE_STAGE_UNSIGNED:-}" == 1 ]]; then
  failed="0 unsigned self-test (no person proofs exist without a signing team)"
elif [[ -z "$team" || "$team" == "not set" ]]; then
  echo "FAIL  0 signed release: $senated is not signed with a team"
  echo "NOT READY: 0 signed release"
  exit 1
fi
version="$("$senate" --version 2>&1)"
echo "Checking $version (team $team) from $rel"

home="$(mktemp -d /tmp/sstage.XXXXXX)" || die "cannot make a staged home"
home="$(cd "$home" && pwd -P)"
label="dev.senate.stage-check.$(uuidgen | tr 'A-Z' 'a-z' | tr -d '-')"
domain="gui/$(id -u)"
service="dev.senate.senated.person-approval.$(printf %s "$home" | shasum -a 256 | cut -c1-24)"
mkdir -p "$home/bin" "$home/work"
legacy_pid=""

cleanup() {
  launchctl bootout "$domain/$label" >/dev/null 2>&1
  # The staged hosts and the stand-in sessions, by the IDs they recorded.
  local pid
  for pid in $(cat "$home"/fake.pids 2>/dev/null) \
    $(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$home"/hosts/*/host-process 2>/dev/null) \
    $legacy_pid; do
    kill "$pid" 2>/dev/null
  done
  if [[ -n "$failed" ]]; then
    echo "The staged home is kept for inspection: $home (senated.log)"
  else
    rm -rf "$home"
  fi
}
trap cleanup EXIT

# The staged daemon's configuration: its own local Senate, and the stand-in
# as the program a hosted Claude session runs.
cat > "$home/senate.toml" <<EOF
person = "stage"
[local]
name = "stage-check"
listen = "127.0.0.1:0"
[companion]
claude_bin = "$home/bin/claude"
EOF

# One MCP tools/call through `senate mcp`; the JSON-RPC reply line in $4.
cat > "$home/bin/mcp-call" <<'EOF'
#!/bin/sh
# usage: mcp-call <senate> <tool> <arguments json> <out file>
fifo="$4.fifo"
mkfifo "$fifo" || exit 1
"$1" mcp <"$fifo" >"$4.log" 2>&1 &
server=$!
exec 3>"$fifo"
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"stage-check","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"$2\",\"arguments\":$3}}" >&3
i=0
while [ $i -lt 900 ] && ! grep -q '"id":2' "$4.log"; do sleep 0.1; i=$((i + 1)); done
exec 3>&-
wait $server
rm -f "$fifo"
grep '"id":2' "$4.log" >"$4.tmp"
mv "$4.tmp" "$4"
EOF

# The stand-in for Claude Code in a staged hosted terminal: it registers
# as Claude's SessionStart hook does, waits for the go-ahead, uses its
# Senate tools (the first one, the chair, also spawns a member), and idles.
cat > "$home/bin/claude" <<EOF
#!/bin/sh
home="$home"
senate="$senate"
EOF
cat >> "$home/bin/claude" <<'EOF'
sid=""
for arg in "$@"; do
  case "$arg" in --session-id=*) sid="${arg#--session-id=}" ;; --resume=*) sid="${arg#--resume=}" ;; esac
done
echo $$ >>"$home/fake.pids"
export SENATE_HOME="$home" CLAUDE_CODE_SESSION_ID="$sid" CLAUDE_PID=$$
printf '{"session_id":"%s","cwd":"%s","transcript_path":"/tmp/%s.jsonl","hook_event_name":"SessionStart","source":"startup"}' \
  "$sid" "$PWD" "$sid" | "$senate" hook session-start >"$home/hook-$sid.out" 2>&1
while [ ! -e "$home/go" ]; do sleep 0.2; done
if mkdir "$home/chair.lock" 2>/dev/null; then
  "$home/bin/mcp-call" "$senate" inbox '{}' "$home/chair-inbox.out"
  "$home/bin/mcp-call" "$senate" spawn \
    "{\"action\":\"spawn\",\"runtime\":\"claude\",\"committee\":\"stage\",\"cwd\":\"$home/work\",\"alias\":\"member\"}" \
    "$home/chair-spawn.out"
fi
exec sleep 3600
EOF
# The runtime of a native session from before the upgrade: its hook and
# MCP clients run under it, as under Claude Code, with the old client when
# one was given (0.12.3 attaches no caller proof).
cat > "$home/bin/legacy-runtime" <<EOF
#!/bin/sh
home="$home"
client="${old:-$senate}"
EOF
cat >> "$home/bin/legacy-runtime" <<'EOF'
export SENATE_HOME="$home" CLAUDE_CODE_SESSION_ID=stage-legacy CLAUDE_PID=$$
while [ ! -e "$home/legacy-go" ]; do sleep 0.2; done
seen() { sqlite3 "$home/state.db" "SELECT last_seen FROM sessions WHERE session_id = 'stage-legacy'"; }
before="$(seen)"
sleep 1
printf '{"session_id":"stage-legacy","cwd":"%s","transcript_path":"/tmp/stage-legacy.jsonl","hook_event_name":"UserPromptSubmit","prompt":"stage check"}' \
  "$home/work" | "$client" hook user-prompt-submit >/dev/null 2>&1
after="$(seen)"
"$home/bin/mcp-call" "$client" inbox '{}' "$home/legacy-inbox.out.part"
echo "$before $after" >"$home/legacy-hook.out"
mv "$home/legacy-inbox.out.part" "$home/legacy-inbox.out"
exec sleep 3600
EOF
chmod 700 "$home/bin/claude" "$home/bin/mcp-call" "$home/bin/legacy-runtime"

# A home as 0.12.3 leaves it: its state schema, and that native session,
# running since before the upgrade.
"$home/bin/legacy-runtime" &
legacy_pid=$!
now_ms="$(($(date +%s) * 1000))"
sqlite3 "$home/state.db" <"$schema" || die "cannot seed the 0.12.3 state database"
sqlite3 "$home/state.db" "PRAGMA user_version = 1;
  INSERT INTO sessions (session_id, cwd, joined_committee, committee_source, claude_pid,
    state, started_at, last_seen, runtime, agent, agent_source)
  VALUES ('stage-legacy', '$home/work', 'stage', 'join', $legacy_pid,
    'live', $now_ms, $now_ms, 'claude', 'claude-stage-legacy', 'harness');" \
  || die "cannot seed the 0.12.3 session"

# The code pins install would record, in the staged home only.
SENATE_HOME="$home" "$senate" setup service pin --senated "$senated" >/dev/null \
  || die "cannot record the code pins in $home"

cat > "$home/job.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key><array>
    <string>$senated</string><string>--machine</string><string>--home</string><string>$home</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>SENATE_HOME</key><string>$home</string>
    <key>HOME</key><string>$home</string>
    <key>PATH</key><string>$home/bin:/usr/bin:/bin</string>
  </dict>
  <key>MachServices</key><dict>
    <key>$service</key><true/>
    <key>$service.scope</key><true/>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$home/senated.log</string>
  <key>StandardErrorPath</key><string>$home/senated.log</string>
</dict></plist>
EOF
launchctl bootstrap "$domain" "$home/job.plist" || die "launchctl bootstrap failed"
for _ in $(seq 100); do [[ -S "$home/bridge.sock" ]] && break; sleep 0.1; done
[[ -S "$home/bridge.sock" ]] || { fail "0 staged daemon" "it did not open $home/bridge.sock; see $home/senated.log"; echo "NOT READY: $failed"; exit 1; }
echo "Staged daemon $label ($home)"

cli() { (cd "$home/work" && SENATE_HOME="$home" "$senate" "$@") 2>&1; }

# 1. A read-only operator request, and opening the staged committee.
out="$(cli observe token list)"
if [[ "$out" == "[]" ]]; then
  pass "1 operator request from this terminal"
else
  fail "1 operator request from this terminal" "$out"
fi
cli join stage >/dev/null

# 2. A sensitive request: authenticate when macOS asks.
echo "      Authenticate when macOS asks (Touch ID or your password): spawning a staged chair."
out="$(cli spawn claude --committee stage --alias lead --chair --dir "$home/work")"
chair=$?
if [[ $chair -eq 0 ]]; then
  pass "2 sensitive request authenticated (staged chair spawned)"
else
  fail "2 sensitive request authenticated" "$out"
fi
touch "$home/go"

# Up to 90 s for a stand-in's result; none when no chair started.
wait_for() {
  [[ $chair -eq 0 ]] || return 1
  for _ in $(seq 900); do [[ -s "$1" ]] && return 0; sleep 0.1; done
  return 1
}
tool_ok() { grep -q '"result"' "$1" && ! grep -q '"isError":true' "$1"; }

# 3. The hosted session the staged daemon started uses its MCP tools.
if wait_for "$home/chair-inbox.out" && tool_ok "$home/chair-inbox.out"; then
  pass "3 hosted session uses its MCP tools"
else
  fail "3 hosted session uses its MCP tools" "$(cat "$home/chair-inbox.out" "$home/chair-inbox.out.log" 2>/dev/null)"
fi

# 4. That chair spawns a member through MCP.
if wait_for "$home/chair-spawn.out" && tool_ok "$home/chair-spawn.out"; then
  pass "4 chair spawns a member through MCP"
else
  fail "4 chair spawns a member through MCP" "$(cat "$home/chair-spawn.out" "$home/chair-spawn.out.log" 2>/dev/null)"
fi

# 5. The session from before the upgrade keeps its hook and MCP tools.
# A served hook records the session as just seen (last_seen changes).
touch "$home/legacy-go"
chair=0
wait_for "$home/legacy-inbox.out"
read -r before after <"$home/legacy-hook.out" 2>/dev/null
if [[ -n "${after:-}" && "${after:-}" != "${before:-}" ]] && tool_ok "$home/legacy-inbox.out"; then
  pass "5 pre-upgrade session keeps its hook and MCP tools ($([[ -n "$old" ]] && echo "old client: $("$old" --version 2>&1)" || echo "this release's client"))"
else
  fail "5 pre-upgrade session keeps its hook and MCP tools" "hook: last seen ${before:-?} -> ${after:-?} | mcp: $(cat "$home/legacy-inbox.out" "$home/legacy-inbox.out.part.log" 2>/dev/null)"
fi

if [[ -n "$failed" ]]; then
  echo "Refused proofs in the staged daemon's log:"
  grep -i "refused" "$home/senated.log" | tail -5
  echo "NOT READY: $failed"
  exit 1
fi
echo "READY TO INSTALL"

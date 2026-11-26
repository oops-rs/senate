#!/bin/bash
# Staged acceptance check of a signed Senate release, run by the person from
# their own terminal BEFORE anything is installed live.
#
#   deploy/stage-check.sh <signed release dir> [0.12.3 senate binary]
#
# <signed release dir> holds the signed `senate`, `senated` and Senate.app
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
#   6. the release's Senate.app, pinned in the staged home, watches the
#      staged daemon (its own launchd job and headless: no window, Dock
#      icon or notification; its menu bar icon shows for a moment)
#   7. `senate session watch` from this terminal shows a staged hosted
#      terminal (one frame, in a pseudo-terminal)
#
# It cleans up by the exact launchd labels and the recorded process IDs, and
# ends with one line: READY TO INSTALL, or NOT READY with the failing step.

set -u

die() { echo "stage-check: $*" >&2; exit 2; }

[[ "$(uname)" == Darwin ]] || die "macOS only"
[[ $# -ge 1 && $# -le 2 ]] || die "usage: deploy/stage-check.sh <signed release dir> [0.12.3 senate binary]"
rel="$(cd "$1" 2>/dev/null && pwd -P)" || die "no such directory: $1"
senate="$rel/senate"
senated="$rel/senated"
app="$rel/Senate.app"
app_exe="$app/Contents/MacOS/Senate"
old="${2:-}"
[[ -x "$senate" && -x "$senated" && -x "$app_exe" ]] || die "$rel must hold senate, senated and Senate.app"
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
# to check this script; such a run never says READY. A debug daemon then
# serves the staged home as a test fixture (no caller checks), so every
# step's own mechanics run.
unsigned=""
app_pin=(--app "$app")
if [[ "${SENATE_STAGE_UNSIGNED:-}" == 1 ]]; then
  failed="0 unsigned self-test (no person proofs exist without a signing team)"
  unsigned=1
  # An unsigned build records no pins, and refuses to pin an app.
  app_pin=()
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
app_label="$label.app"
mkdir -p "$home/bin" "$home/work"
legacy_pid=""
app_pid=""
watch_pid=""

cleanup() {
  launchctl bootout "$domain/$app_label" >/dev/null 2>&1
  launchctl bootout "$domain/$label" >/dev/null 2>&1
  # The staged hosts and the stand-in sessions, by the IDs they recorded.
  local pid
  for pid in $(cat "$home"/fake.pids 2>/dev/null) \
    $(sed -n 's/.*"pid":\([0-9]*\).*/\1/p' "$home"/hosts/*/host-process 2>/dev/null) \
    $legacy_pid $app_pid $watch_pid; do
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
# On its terminal's screen, for step 7's viewer to find.
echo "stage-check hosted terminal"
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

# The code pins install would record, the daemon's and the app's, in the
# staged home only.
SENATE_HOME="$home" "$senate" setup service pin --senated "$senated" ${app_pin[@]+"${app_pin[@]}"} >/dev/null \
  || die "cannot record the code pins in $home"
confine=""
[[ -z "$unsigned" ]] || confine="<key>SENATE_CONFINE_TO</key><string>$home</string>"

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
    $confine
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
spawned=$chair
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

# 6. The release's app watches the staged daemon, as the person's app
# watches theirs. It is its own launchd job, as when opened from Finder:
# macOS then holds it responsible for itself, so only the app pin can admit
# it (started from this shell it would be served as this terminal). Its
# core says what came of the watch on stderr, in the staged home.
# (A Debug build keeps its code in a dylib beside the executable.)
if ! grep -aqsF SENATE_APP_HEADLESS "$app_exe" "$app_exe.debug.dylib"; then
  fail "6 the app watches the staged daemon" "$app predates SENATE_APP_HEADLESS (it would show in the Dock and could update the installed app's scenes): check a release built from this checkout"
else
  cat > "$home/app.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>$app_label</string>
  <key>ProgramArguments</key><array><string>$app_exe</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>SENATE_HOME</key><string>$home</string>
    <key>SENATE_APP_HEADLESS</key><string>1</string>
  </dict>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$home/app.log</string>
  <key>StandardErrorPath</key><string>$home/app.log</string>
</dict></plist>
EOF
  watching="watching the daemon on $home/bridge.sock"
  if launchctl bootstrap "$domain" "$home/app.plist"; then
    # Until the core has watched or given up once, or the app is gone
    # (30 s at most).
    for _ in $(seq 300); do
      [[ -n "$app_pid" ]] || app_pid="$(launchctl print "$domain/$app_label" 2>/dev/null | sed -n 's/^[[:space:]]*pid = \([0-9]*\)$/\1/p' | head -1)"
      grep -aqF -e "$watching" -e "daemon unreachable:" "$home/app.log" 2>/dev/null && break
      [[ -z "$app_pid" ]] || kill -0 "$app_pid" 2>/dev/null || break
      sleep 0.1
    done
  fi
  if grep -aqF "$watching" "$home/app.log" 2>/dev/null; then
    pass "6 the app watches the staged daemon ($(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist" 2>/dev/null), pid ${app_pid:-?})"
  else
    fail "6 the app watches the staged daemon" "$( (grep -aF "daemon unreachable:" "$home/app.log" || tail -5 "$home/app.log") 2>/dev/null | head -3)"
  fi
  launchctl bootout "$domain/$app_label" >/dev/null 2>&1
  [[ -z "$app_pid" ]] || kill "$app_pid" 2>/dev/null
  # Ended: cleanup must not signal whatever reuses the ID.
  app_pid=""
fi

# 7. `senate session watch` from this terminal: a person-only watch, with
# read-only viewers of the staged hosted terminals. One frame, in a
# pseudo-terminal of its own, then q.
if [[ $spawned -ne 0 ]]; then
  fail "7 session watch from this terminal" "no staged hosted terminal to watch (step 2)"
else
  # The viewer's keys come down a pipe (script(1) refuses a FIFO): q once a
  # tile shows the stand-in's line or why it could not watch, or the viewer
  # is gone (30 s at most).
  {
    for _ in $(seq 300); do
      grep -aqF -e "stage-check hosted terminal" -e "which process sent this request" "$home/watch.out" 2>/dev/null && break
      [[ ! -e "$home/watch.done" ]] || break
      sleep 0.1
    done
    printf q
  } | (cd "$home/work" && SENATE_HOME="$home" exec script -q -F "$home/watch.out" \
    /bin/sh -c 'stty rows 30 cols 160; exec "$0" session watch' "$senate") >/dev/null 2>&1 &
  watch_pid=$!
  for _ in $(seq 350); do kill -0 "$watch_pid" 2>/dev/null || break; sleep 0.1; done
  touch "$home/watch.done"
  kill "$watch_pid" 2>/dev/null
  watch_pid=""
  if grep -aqF "stage-check hosted terminal" "$home/watch.out" 2>/dev/null; then
    pass "7 session watch from this terminal"
  else
    # The screen without its escape sequences.
    fail "7 session watch from this terminal" "$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g; s/\e[()=>].?//g; s/[^[:print:]\n]+/ /g; s/ {2,}/ /g' "$home/watch.out" 2>/dev/null | grep -v '^ *$' | head -5)"
  fi
fi

if [[ -n "$failed" ]]; then
  echo "Refused proofs in the staged daemon's log:"
  grep -i "refused" "$home/senated.log" | tail -5
  echo "NOT READY: $failed"
  exit 1
fi
echo "READY TO INSTALL"

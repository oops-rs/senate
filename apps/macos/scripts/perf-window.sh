#!/usr/bin/env bash
# Measures a running Senate.app over a window of time: how many snapshots its
# core pushed and what they cost, the app's CPU, its main-thread stalls and
# its memory. It only reads: `ps` for the process, and the app's stderr,
# which must have been started with SENATE_APP_PERF=watch (the app prints
# its measures and stays) and sent to a file. It starts and ends nothing.
#
#   SENATE_HOME=/tmp/sh SENATE_APP_SHOW=1 SENATE_APP_PERF=watch SENATE_APP_NOTIFICATIONS=0 \
#       build/Build/Products/Release/Senate.app/Contents/MacOS/Senate 2> /tmp/sh/app.log &
#   scripts/perf-window.sh $! /tmp/sh/app.log          # waits 10 s, measures 30 s
#
# Usage: scripts/perf-window.sh <app pid> <app stderr log> [settle seconds] [window seconds]
set -euo pipefail

pid="${1:?the app pid}"
log="${2:?the file the app stderr goes to}"
settle="${3:-10}"
window="${4:-30}"

now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time * 1000'; }
# `ps -o time=` is [[h:]m:]s.cc of CPU the process has used.
cpu_s() { ps -o time= -p "$pid" | awk -F: '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; printf "%.2f\n", s }'; }

sleep "$settle"
t0="$(now_ms)"
c0="$(cpu_s)"
sleep "$window"
t1="$(now_ms)"
c1="$(cpu_s)"
rss_kb="$(ps -o rss= -p "$pid" | tr -d ' ')"

grep -a 'first online' "$log" || echo "no first online state in $log"
awk -v t0="$t0" -v t1="$t1" -v c0="$c0" -v c1="$c1" -v rss="$rss_kb" '
  function field(name,    i, kv) {
    for (i = 1; i <= NF; i++) { split($i, kv, "="); if (kv[1] == name) return kv[2] + 0 }
    return 0
  }
  /^perf: core at_ms=/ {
    at = field("at_ms")
    if (at < t0 || at > t1) { inside = 0; next }
    if (!first) {
      first = at
      split("changed pushed unchanged clone_us build_us compare_us deliver_us", keys, " ")
      for (k in keys) from[keys[k]] = field(keys[k])
    } else {
      wc = field("clone_worst_us"); if (wc > worst_clone) worst_clone = wc
      wb = field("build_worst_us"); if (wb > worst_build) worst_build = wb
      wk = field("compare_worst_us"); if (wk > worst_compare) worst_compare = wk
      wd = field("deliver_worst_us"); if (wd > worst_deliver) worst_deliver = wd
    }
    last = at
    for (k in keys) to[keys[k]] = field(keys[k])
    inside = 1
    next
  }
  inside && /^perf: main-thread worst stall/ { stalls = stalls " " $5; if ($5 + 0 > worst_stall) worst_stall = $5 + 0; next }
  inside && /^perf: apply/ { applies++; apply_ms += $(NF - 1) }
  END {
    if (!first || last == first) { print "no core lines in the window: was the app started with SENATE_APP_PERF=watch?"; exit 1 }
    secs = (last - first) / 1000
    pushed = to["pushed"] - from["pushed"]
    built = pushed + to["unchanged"] - from["unchanged"]
    printf "core, over %.1f s: changed %d (%.1f/s), snapshots built %d, unchanged and not pushed %d, pushed %d (%.2f/s)\n",
      secs, to["changed"] - from["changed"], (to["changed"] - from["changed"]) / secs, built, built - pushed, pushed, pushed / secs
    if (built > 0) printf "  per snapshot built: clone mean %.0f us (worst %d), build mean %.0f us (worst %d), compare mean %.0f us (worst %d)\n",
      (to["clone_us"] - from["clone_us"]) / built, worst_clone, (to["build_us"] - from["build_us"]) / built, worst_build,
      (to["compare_us"] - from["compare_us"]) / built, worst_compare
    if (pushed > 0) printf "  per snapshot pushed: deliver mean %.0f us (worst %d)\n",
      (to["deliver_us"] - from["deliver_us"]) / pushed, worst_deliver
    spent = 0
    split("clone_us build_us compare_us deliver_us", costs, " ")
    for (k in costs) spent += to[costs[k]] - from[costs[k]]
    printf "  core time on snapshots: %.1f ms per second\n", spent / 1000 / secs
    printf "app: states applied %d (%d ms in apply)\n", applies, apply_ms
    printf "app: CPU %.2f s over %.1f s = %.1f%% of one core\n", c1 - c0, (t1 - t0) / 1000, 100 * (c1 - c0) / ((t1 - t0) / 1000)
    printf "app: main-thread worst stall %d ms (each 2 s:%s)\n", worst_stall, stalls
    printf "app: RSS %.0f MB\n", rss / 1024
  }
' "$log"

# Measuring Senate

Everything here measures a machine of its own: a Senate home under
`/tmp/senate-perf` with its own `HOME`, a free loopback port and nothing of
the calling session's environment. It never reads or writes `~/.senate` and
never talks to the daemon you use. What it starts is recorded by pid and
`stop` ends those pids only.

## The headline numbers

```sh
cargo build --profile perf -p senated -p senate-bridge --bins
scripts/perf/bench.py setup            # 30 committees, 20 hosted sessions, 40 past ones, 3000 events
scripts/perf/bench.py run --json before.json
scripts/perf/bench.py stop --remove
```

`setup` takes `--committees`, `--sessions`, `--past`, `--events` and
`--turns` (the length of each session's transcript). `run` takes
`--only idle,cli,delivery,watch,flood` to take one part, `--idle-secs` and
`--runs`.

A hosted session's TUI is a shell script. It reports itself with the
session-start hook and a transcript, as Claude Code does, and then waits. A
helper below it runs the hooks the benchmark asks for, so they are timed
where a real session's are: inside the session's own process tree.

What `run` prints:

| Part | What it is |
| :--- | :--- |
| Idle | The daemon's and the hosts' CPU over a quiet minute, context switches a second, memory, threads, descriptors. CPU is each process's own: a process it starts (`git`) is not counted. |
| Latency | Wall time of each command, median, 95th percentile and minimum over `--runs` runs. `hook user-prompt-submit` is what every turn of a hosted agent waits for. `ask -> queued` is from posting a question to the moment the session's next hook carries it. |
| Watch grid | `senate session watch` over every hosted terminal for ten seconds: its own CPU, and the daemon's and hosts' while it is open. |
| Relay throughput | A hosted TUI printing 20 MiB as fast as its terminal takes it. A bare PTY reader on the same machine is the ceiling to compare with. |
| Sizes | The databases with their write-ahead logs, and the binaries. |

To compare a change, take both runs with the same build profile on the same
machine, one after the other. On a busy machine read the minimum, not the
median: wall time includes waiting for a core. CPU time, context switches
and counts do not depend on the load.

### Why the `perf` profile

An unsigned daemon takes a person's requests (`spawn`, `ask`) only as a
debug fixture confined to a temp home (`SENATE_CONFINE_TO`). `perf` is the
release profile with debug assertions on, so the fixture works, and with
line tables, so a profiler names source lines. Its numbers are a little
above a signed release's where code carries debug assertions or overflow
checks, and leave out the signed build's caller attestation.

## Where the time goes

```sh
sample "$(cat /tmp/senate-perf/pids/daemon)" 30 1 -file daemon.sample.txt   # macOS
```

Read the call graph for the frames that are not waiting. On Linux,
`perf record -g -p <pid> -- sleep 30`.

## Query plans

```sh
scripts/perf/query_plans.py crates/senate-bridge/src/state.rs /tmp/senate-perf/h/.senate/state.db
scripts/perf/query_plans.py crates/senate-log/src/sqlite.rs /tmp/senate-perf/h/.senate/senates/top/log.db
```

Prints every statement in the source file that scans a table or sorts with
a temporary B-tree, with the table's row count in the fixture, largest
first. `--all` prints every plan.

## Stack depth

```sh
scripts/perf/stack_depth.py                      # the optimised daemon
RUST_MIN_STACK=1048576 cargo test -p senate-bridge --test hosted   # the debug build, half the usual stack
```

The first starts the daemon with smaller and smaller thread stacks and asks
each for a hosted session, the deepest path in the daemon. The second runs
the hosted tests with 1 MiB worker stacks instead of 2 MiB: it passes while
the debug build's deepest path fits in half its stack. To see which futures
are large:

```sh
RUSTC_BOOTSTRAP=1 cargo rustc -p senate-bridge --lib -- -Zprint-type-sizes | grep 'async fn body' | sort -t: -k3 -n
```

A future is as large as everything it keeps across an await, and so is
every future that awaits it. An array on the stack of an `async fn`, alive
across an await, is the usual cause.

## Build

```sh
cargo build --release --workspace --bins --timings
cargo test --workspace --no-run --timings        # then: du -sh target/debug
```

`target/cargo-timings/` has each unit's time.

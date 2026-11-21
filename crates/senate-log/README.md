# senate-log

[![crates.io](https://img.shields.io/crates/v/senate-log.svg)](https://crates.io/crates/senate-log) [![docs.rs](https://img.shields.io/docsrs/senate-log)](https://docs.rs/senate-log)

The committee log: one append-only log per committee, with cursors, mirror refs and heartbeats.

## Stores

`LogStore` is the seam; `connect` picks an implementation from a URL:

| URL | Store | For |
|---|---|---|
| `memory:` | `MemoryStore` | tests and dev mode |
| `sqlite:<path>` | `SqliteStore` | a developer machine (one embedded file) |
| `postgres://…` | `PgStore` | a server (sqlx, embedded migrations) |

All three pass the same conformance suite. Besides the events keyed by `(committee, seq)`, a store keeps the small side tables a restart needs: named cursors, outbound mirror refs, and the latest bridge heartbeat per agent.

The server is the only component that holds a `LogStore`; everything else reaches the log through `senate_core::Hub`.

```toml
[dependencies]
senate-log = "0.3"
```

## Part of Senate

Senate gives a team's coding agents (Claude Code, Codex) and the people who run them one shared, append-only log per committee: agents ask, answer and hand work to each other, and a Speaker relays to and from chat. See the [Senate repository](https://github.com/oops-rs/senate) for the architecture and the [README](https://github.com/oops-rs/senate#readme) to get started.

| Crate | Role |
|---|---|
| [`senate`](https://crates.io/crates/senate) | The `senate` command (install this) |
| [`senated`](https://crates.io/crates/senated) | The daemon, and the library to build your own |
| [`senate-bridge`](https://crates.io/crates/senate-bridge) | The developer machine's side: sessions, delivery, MCP, hooks |
| [`senate-server`](https://crates.io/crates/senate-server) | HTTP/SSE server over the log |
| [`senate-speaker`](https://crates.io/crates/senate-speaker) | The built-in Speaker |
| [`senate-log`](https://crates.io/crates/senate-log) | The committee log stores |
| [`senate-work`](https://crates.io/crates/senate-work) | Coding work on request: runner and forge |
| [`senate-core`](https://crates.io/crates/senate-core) | Shared contracts and wire types |

## License

MIT

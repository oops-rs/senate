# senate-server

[![crates.io](https://img.shields.io/crates/v/senate-server.svg)](https://crates.io/crates/senate-server) [![docs.rs](https://img.shields.io/docsrs/senate-server)](https://docs.rs/senate-server)

The Senate server: the supervision tree over the log, the HTTP/SSE API for bridges, and the `Hub` in-process components share.

## What it does

- **HTTP/SSE API** (`senate_core::api`): bridges append events, read pages, stream a committee live, keep cursors and heartbeats, and register child Senates.
- **Who is calling**: a bearer token names a person, who acts as themselves or one of their agents; the roster checks it, and a new agent is seated on first contact.
- **Who presides**: the Speaker's seat, the docket of what awaits it, chairs, secretaries and committee seats.
- **Child Senates**: a Senate one level down registers with its parent and declares its seats (senator, duty, presence), so the parent can route to them.
- **`ServerHub`**: the `senate_core::Hub` that in-process components (a chat gateway, the built-in Speaker) share with the HTTP layer.

Most people run it through [`senated`](https://crates.io/crates/senated) rather than embedding it directly.

```toml
[dependencies]
senate-server = "0.5"
```

## Part of Senate

Senate gives a team's coding agents (Claude Code, Codex, agy) and the people who run them one shared, append-only log per committee: agents ask, answer and hand work to each other, and a Speaker relays to and from chat. See the [Senate repository](https://github.com/oops-rs/senate) for the architecture and the [README](https://github.com/oops-rs/senate#readme) to get started.

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

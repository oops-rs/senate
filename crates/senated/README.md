# senated

[![crates.io](https://img.shields.io/crates/v/senated.svg)](https://crates.io/crates/senated) [![docs.rs](https://img.shields.io/docsrs/senated)](https://docs.rs/senated)

The Senate daemon, `senated`, and the library a private layer builds its own daemon from.

## Run it

```sh
cargo install senate senated
senated --machine            # on a developer machine (usually via `senate setup service install`)
senated --config server.toml # one Senate on a server
```

The daemon is the same at every level. On a server it is one Senate; on a developer machine it is the Senates hosted there plus the machine's side of every Senate it serves. A machine can register its Senate as a child of a team's Senate, and only what one addresses to the other crosses.

## Build your own

Senate has no chat platform of its own. A layer that connects it to a company's systems hands `Assembly` what it brings (a chat surface, authentication, a roster, extra components) and gets a daemon that is `senated` plus its parts:

- the log and the server (`memory:`, `sqlite:<path>` or `postgres://…`),
- the built-in Speaker when a model is configured,
- the machine side (bridge, uplink to a parent Senate) when run with `--machine`.

```toml
[dependencies]
senated = "0.4"
```

## A chat platform in its own process

A platform adapter can also run as a separate process and attach over a local Unix socket (`[chat_gateway]` in the config). The socket protocol carries every `ChatSurface` operation, so this two-process mode can do everything the in-process mode does. See [docs/chat-gateway-ipc.md](https://github.com/oops-rs/senate/blob/main/docs/chat-gateway-ipc.md) for the complete wire specification.

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

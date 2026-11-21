# senate-speaker

[![crates.io](https://img.shields.io/crates/v/senate-speaker.svg)](https://crates.io/crates/senate-speaker) [![docs.rs](https://img.shields.io/docsrs/senate-speaker)](https://docs.rs/senate-speaker)

The built-in Speaker: it presides over a Senate, backed by a model, as the only voice on the chat platform.

## What it does

- **Interprets** chat addressed to it, with a model or the fixed syntax that always works (`/bind`, `/unbind`, `/tell`, `/status`, `/decide`, `/brief`, `/agents`, `/invite`, `/output`, `/work`, `/reassign`).
- **Routes to agents, never guesses them**: "Alice's agent" is a roster lookup; several candidates are settled by what each is doing or by asking. An unaddressed question about a subject goes to the one seat whose duty covers it, here or in a child Senate; questions about the Senate itself it answers directly.
- **Relays, mirrors and triages** between the log and the chat platform, answers for agents that are away from their checkpoints, and decides what to do with a question for a child Senate that is offline.
- **Does coding work on request** through [`senate-work`](https://crates.io/crates/senate-work).

Runs inside [`senated`](https://crates.io/crates/senated) when a model is configured.

```toml
[dependencies]
senate-speaker = "0.4"
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

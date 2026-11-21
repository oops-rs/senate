# senate-core

[![crates.io](https://img.shields.io/crates/v/senate-core.svg)](https://crates.io/crates/senate-core) [![docs.rs](https://img.shields.io/docsrs/senate-core)](https://docs.rs/senate-core)

Senate's contracts: everything that crosses a crate or process boundary, defined once.

## What's in it

- **Names and addresses**: committee ids, `seq`, event ids; `SenatorName` (a person such as `wendell` or an agent such as `claude`); `SenatePath` (`acme/north-desk`); addresses (`senator::<name>`, `senator::*`, `speaker`, `committee`, `guest::<platform>/<id>`); qualified locators (`acme/north-desk::ledger::senator::codex`) and in-text mentions (`[senator::claude]`).
- **Aliases**, each on the committee it was given in, and how a bare or qualified alias resolves.
- **The roster**: who sits in a Senate, who may act as whom, and what "whichever agent" resolves to.
- **The event envelope**: the single record of a committee's log, its payloads and shared `meta` keys.
- **The wire**: the HTTP/SSE API types (`api`) used by the server and every client.
- **Seams**: the `Hub` trait that components read and write the log through, and the chat-surface seam a platform plugs into.
- **Derived views**: briefs, continuity and presence, tasks, dockets and duties.

## Who needs it

Anyone writing a Senate component or client in Rust: a chat gateway, a custom Speaker, a tool that reads the log. It has no I/O of its own.

```toml
[dependencies]
senate-core = "0.4"
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

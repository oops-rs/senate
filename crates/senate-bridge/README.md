# senate-bridge

[![crates.io](https://img.shields.io/crates/v/senate-bridge.svg)](https://crates.io/crates/senate-bridge) [![docs.rs](https://img.shields.io/docsrs/senate-bridge)](https://docs.rs/senate-bridge)

The developer machine's side of Senate: a person's coding-agent sessions joined to committee logs, and the `senate` command.

## Install the command

The command is published as the [`senate`](https://crates.io/crates/senate) package, which installs the same binary:

```sh
cargo install senate senated
senate setup install --yes          # register Senate with Claude Code and Codex
senate setup service install --yes  # keep the daemon running, from login
senate join                         # in each repository: bind it to a committee
```

## What it does

- **Sessions and delivery**: Claude Code and Codex sessions in a worktree join its committee; messages for an agent are pushed into its session, or held for its next hook.
- **MCP tools and hooks** the coding agents call: ask, answer, status, checkpoint, join, duty, and the chair's and Speaker's tools for a session that holds those seats.
- **Hosted sessions**: `senate spawn` starts an agent in a Senate-owned terminal; `senate attach` opens it (`--read-only` to look), and `senate session list|watch|stop|restore` lists them, shows every one side by side, and ends or reopens one.
- **A worktree's `.senate/`** follows its branch.

It runs inside `senated --machine`; depend on this crate to embed the machine side in your own daemon.

```toml
[dependencies]
senate-bridge = "0.4"
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

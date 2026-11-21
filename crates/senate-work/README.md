# senate-work

[![crates.io](https://img.shields.io/crates/v/senate-work.svg)](https://crates.io/crates/senate-work) [![docs.rs](https://img.shields.io/docsrs/senate-work)](https://docs.rs/senate-work)

The Speaker's hands: coding work done on a person's request, on a branch of the Speaker's own, delivered as that branch or a pull request.

## Pieces

- **`WorkOrder`**: what a person asked for, whose checkpoint it starts from, and which branch it lands on. Only listed repositories and the committee's Speaker branches (`<prefix>/<committee>/speaker-<n>`) are allowed; never a developer's own branch.
- **`Runner`**: does the order and reports progress steps. `DockerRunner` runs Claude Code headless in a container over a throwaway checkout, and commits and pushes even when a run fails or times out.
- **`Forge`**: opens the pull request. `GhForge` uses `gh`; `NoForge` delivers the branch alone.
- **`testing`** (feature `testing`): fake runner and forge.

Used by the built-in Speaker when `[speaker.work]` is configured.

```toml
[dependencies]
senate-work = "0.4"
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

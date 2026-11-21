# senate

The `senate` command of [Senate](https://github.com/oops-rs/senate): reach
your coding agents, see what waits for you, and run the hooks and MCP server
that Claude Code, Codex and the Antigravity CLI (agy) use.

```
cargo install senate
```

This package only carries the name. The command is implemented in
[`senate-bridge`](https://crates.io/crates/senate-bridge), and both packages
install the same `senate` binary, so install one of them, not both. The daemon
is a separate package, [`senated`](https://crates.io/crates/senated).

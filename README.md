# Senate

A deliberation system for people and their coding agents. Work is divided
into committees; each committee has one shared, append-only log, and people
and agents read and write the same record, so what one agent learned is not
locked inside one machine.

人和编码 agent 共用的议事系统。工作按 committee 划分，每个 committee 一份只追加的日志，人和 agent 读写同一份记录。一个人可以有多个 agent（Claude Code、Codex……），每个都是独立的 senator；Speaker 由模型驱动，负责分流问题、在 agent 不在时依据它自己的检查点代答。`senated` 单独运行即可使用；接入聊天平台或 agent 网关的部分由私有层基于 `senated` 库自行组装，不在本仓库中。

## Vocabulary

Senate, senator and Speaker are used as-is in every language.

| Term | Meaning |
| --- | --- |
| Senate | One level of deliberation, served by one `senated`. Senates nest. |
| senator | Anyone who can speak: a person or an agent. |
| senior senator | A person. Global: `senator::wendell` is the same senator at every level. |
| junior senator | An agent, named under its person: `senator::wendell/claude`. |
| Speaker | Presides over one Senate. Always backed by a model. The Speaker of a nested Senate is, by default, a senator of the Senate above it. |
| Committee | The unit of work: its own log, members and threads. |

An agent's own subagents are not seated beside it. They can sit in a Senate
one level down that the agent presides over:
`senator::wendell/claude/reviewer`.

In running text an id is written in brackets so it can be told apart from the
words around it in any language: `[senator::wendell/claude]`, `[speaker]`,
and `[speaker::wendell/claude]` for the Speaker answering on that senator's
behalf. Someone who is not a senator (a chat participant not on the roster)
is a guest, `guest::<platform>/<id>`: a guest may ask and be answered, and
changes nothing.

## How it fits together

```text
   developer machine                         senated
 ┌───────────────────────┐        ┌─────────────────────────────────────┐
 │ Claude Code ─┐        │  HTTP  │ senate-server ── Hub ──┬─ Speaker   │
 │ Codex ───────┼ senate-├───SSE──┤   (auth, roster,       ├─ chat      │
 │              │ bridge │        │    supervision)        │  gateway*  │
 └──────────────┴────────┘        │        │               └─ other     │
                                  │   senate-log                components*
                                  │ (memory / SQLite / Postgres)        │
                                  └─────────────────────────────────────┘
                                         * mounted by a private layer
```

- **`senated`** keeps the log and serves it over HTTP/SSE. With `[speaker]`
  it runs the built-in Speaker, which triages questions between agents,
  answers for an agent that is away (from that agent's own latest
  checkpoint), hands its mailbox back when it returns, and, with `[work]`,
  does coding work on request in a container.
- **`senate-bridge`**, one daemon per developer machine, authenticates as the
  person and seats one agent per kind of coding session: a Claude Code
  session speaks as `<person>/claude`, a Codex session as `<person>/codex`.
  It delivers each agent's messages into that agent's own sessions and gives
  them MCP tools (`ask`, `answer`, `catch_up`, `search`, `post`,
  `propose_decision`, `checkpoint`, `join`, …).
- **A chat platform** is a private layer: it mounts a chat surface on
  `senated` through the assembly library, and people then bind groups to
  committees, talk to the Speaker, and see agents' threads as cards.

The model and its rules are in [`docs/architecture.md`](docs/architecture.md).

## Quick start

Requires Rust 1.95 or later.

### 1. Run `senated`

```sh
cargo build --release -p senated -p senate-bridge
cp config/senated.example.toml senated.toml   # edit people and tokens
./target/release/senated --config senated.toml
```

The example serves `127.0.0.1:7878` with the log in memory. For a log that
survives a restart, on one machine:

```toml
database_url = "sqlite:/var/lib/senate/log.db"
```

or Postgres for a shared server (`deploy/docker-compose.yml` runs one):

```toml
database_url = "postgres://senate:senate@127.0.0.1:5432/senate"
```

Check it with a token from the config:

```sh
curl -H "Authorization: Bearer dev-alice-token" http://127.0.0.1:7878/v1/me
# {"person":"alice","agents":[],"committees":[],"epoch":"…","triaged":false}
```

Uncomment `[speaker]` to run the built-in Speaker. Its model comes from
basis's environment (`ANTHROPIC_API_KEY`, or `BASIS_BASE_URL` and
`BASIS_API_KEY` for a gateway); without `[speaker]`, agents still ask and
answer each other through the log, and nothing is triaged or covered.

### 2. Install and start the bridge on a developer machine

```sh
mkdir -p ~/.senate && cp config/bridge.example.toml ~/.senate/bridge.toml
chmod 600 ~/.senate/bridge.toml       # set server_url, token, person
./target/release/senate-bridge install          # dry run: prints what it would change
./target/release/senate-bridge install --yes    # registers the MCP server and hooks
./target/release/senate-bridge daemon           # keep it running (your own service manager)
./target/release/senate-bridge status
```

`install` registers the MCP server `senate` and the session hooks with Claude
Code, and with Codex when `codex` is on `PATH`. The daemon is a plain
foreground process; run it under whatever starts your other services.

### 3. Which committee, which agent

A session's committee is, in this order of precedence:

1. the one it joined explicitly (the agent's `join` tool, or `/senate join`),
2. its git branch, when that is `<branch_prefix>/<committee>/<person>`
   (`feature/refund-v2/alice`; the prefix is configurable),
3. a `.senate/committee` file at the root of the worktree holding the
   committee's name.

A session with no committee receives nothing.

It speaks as one agent of the person the token names:

1. `SENATE_AGENT` in a Claude Code session's environment,
2. a `.senate/agent` file at the root of the worktree,
3. `[agents]` in `bridge.toml`, by harness,
4. the harness's own name: `claude` or `codex`.

So one person running Claude Code and Codex side by side is two senators,
`alice/claude` and `alice/codex`; each gets only what is addressed to it. A
message for "Alice's agent" (`senator::alice/*`) reaches her only agent, or
Alice herself when she has none; when she has several, an agent sender is
refused with the candidates and must name one, and a person in chat is asked
by the Speaker which one they mean.

## Mounting a chat platform

A chat platform, an agent gateway and any other company-specific piece are
built on the `senated` library instead of forking it. The platform half of a
chat layer implements `senate_core::ChatSurface` (post, cards, groups,
grants) and receives messages; `senated::ChatGateway` does the rest
(senders to senators or guests via the roster's accounts, bound groups to
the log's `chat` events, threads, unbound mentions, bind = grant):

```rust
use std::sync::Arc;
use senated::{Assembly, ChatGateway, Inbound, SenatedConfig, component_fn};

let config = SenatedConfig::load("senated.toml".as_ref())?;
let platform: Arc<dyn senate_core::ChatSurface> = Arc::new(MyPlatform::connect(&config)?);
let gateway = ChatGateway::new(platform);

let daemon = Assembly::new(config)
    .chat_gateway(gateway.clone())?             // the surface for the Speaker, the gateway as a component
    .auth(Arc::new(MySso::new()))               // optional: your identity system
    .component(component_fn("transport", move |ctx| {
        let gateway = gateway.clone();
        async move {
            // for each message the platform delivers:
            // gateway.deliver(Inbound { … }).await?;
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }))
    .start()
    .await?;
daemon.run_until_signal().await
```

Components get the `Hub` and a shutdown signal, and are supervised with the
Speaker: one that fails is started again, and a set that keeps failing stops
the daemon. Your layer's own config sections sit in the same file and are
read from `config.server.extra`. One chat platform per daemon: the built-in
Speaker speaks on one. `senated`'s `testing` feature has `FakeChat`, a
platform in memory for testing a layer's assembly end to end.

## Crates

| Crate | Role |
| --- | --- |
| `senate-core` | Contracts: ids, senator paths, addresses, mentions, the roster, locales, the event envelope, HTTP/SSE wire types, the `Hub` and `ChatSurface` seams, briefs, continuity, tasks, work runs. |
| `senate-log` | The log store: in memory, SQLite, Postgres. |
| `senate-server` | The supervision tree over the log, the HTTP/SSE API, authentication and roster seams. |
| `senate-speaker` | The built-in Speaker, on basis. |
| `senate-work` | The Speaker's hands: coding work in a container, delivered as a branch or a PR. |
| `senate-bridge` | The developer-machine daemon, hooks and MCP tools for Claude Code and Codex. |
| `senated` | The daemon binary and the assembly library. |
| `senate-e2e` | End-to-end tests of all of the above together. |

Configuration examples: [`config/senated.example.toml`](config/senated.example.toml),
[`config/bridge.example.toml`](config/bridge.example.toml).

```sh
cargo test --workspace
```

## Status and known limits

Early. Every crate is tested on its own and together: the end-to-end suite
runs the real server, the real Speaker, real bridge daemons and the
assembly, over the in-memory and SQLite stores, in Chinese and English.
What has **not** run yet:

- **A real model.** Every test drives the Speaker with a scripted brain.
  The basis brain's prompts, tool use and output parsing are tested against
  a scripted provider, never a live model.
- **Real Claude Code and Codex.** Sessions are fake inbox sockets and a fake
  Codex app-server speaking the protocol as the bridge expects it;
  `install`'s edits are tested on temporary copies of the configuration.
  The companion session runs a stand-in for `claude`.
- **Real Docker and `gh`.** Work runs use a fake runner and forge in the
  end-to-end suite; the Docker runner and the `gh` forge are tested against
  stand-in binaries.
- **Postgres** is tested by the log's conformance suite and the server's
  own tests when `SENATE_TEST_DATABASE_URL` is set; the end-to-end suite
  uses memory and SQLite.
- **A real chat platform.** None is in this repository; the gateway half is
  tested through the fake platform.
- One chat platform per daemon. Nested Senates are built and tested two
  levels deep.

## License

MIT

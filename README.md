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
   developer machine: senated --machine            team server: senated --config
 ┌─────────────────────────────────────────┐      ┌──────────────────────────────┐
 │ Claude Code ─┐   developer-machine side │ HTTP │ senate-server ── Hub ─ Speaker│
 │ Codex ───────┼── (senate-bridge) ───────┼─SSE──┤  (auth, roster,   (built in) │
 │              │        │                 │      │   supervision)    chat gw.*  │
 │              │   hosted Senates:        │      │        │                     │
 │              │   a local top Senate, or │      │   senate-log                 │
 │              │   one per presiding agent│      │ (memory / SQLite / Postgres) │
 │              │   (SQLite, loopback)     │      └──────────────────────────────┘
 └─────────────────────────────────────────┘          * mounted by a private layer
```

- **`senated`** is the daemon, the same at every level. As a team server
  (`senated --config senated.toml`) it keeps the log and serves it over
  HTTP/SSE; with `[speaker]` it runs the built-in Speaker, which triages
  questions between agents, answers for an agent that is away (from that
  agent's own latest checkpoint), hands its mailbox back when it returns,
  and, with `[work]`, does coding work on request in a container. On a
  developer machine (`senated --machine`) it runs that machine's side of
  every Senate it serves, and hosts the Senates that live there.
- **`senate`** is the command for people and agents on a developer machine:
  `install` registers its MCP server and hooks with Claude Code and Codex,
  `status`, `inbox`, `take`, `init --local`, and `mcp` / `hook` are what
  Claude Code and Codex run. A Claude Code session speaks as
  `<person>/claude`, a Codex session as `<person>/codex`; each gets its
  messages pushed into its own sessions and MCP tools (`ask`, `answer`,
  `catch_up`, `search`, `post`, `propose_decision`, `checkpoint`, `join`, …).
- **A chat platform** is a private layer: it mounts a chat surface on
  `senated` through the assembly library, and people then bind groups to
  committees, talk to the Speaker, and see agents' threads as cards.

### Senates nest

An agent's own subagents can sit in a Senate one level down that the agent
presides over: `senator::alice/claude` is an ordinary senator in the Senate
above and the Speaker of its own Senate, where `alice/claude/reviewer`
sits. The presiding session is attached to both: it answers upstairs in its
own name, asks its subagents downstairs as their Speaker, and gets four
presiding tools (`speaker_docket`, `speaker_route`, `speaker_cover`,
`speaker_escalate`) to route messages meant for "one of alice's agents",
answer for an away subagent from that subagent's own checkpoint, and bring
the person what it cannot settle. Nothing is copied between the levels.
Alone on one machine, the top Senate is local and one of your agents is
its Speaker.

The model and its rules are in [`docs/architecture.md`](docs/architecture.md).

## Three ways to run

Requires Rust 1.95 or later.

```sh
cargo build --release -p senated -p senate-bridge   # target/release/senated, target/release/senate
```

### 1. A team server

[`config/senated.example.toml`](config/senated.example.toml): people,
tokens, the log, optionally `[speaker]`.

```sh
cp config/senated.example.toml senated.toml   # edit people and tokens
./target/release/senated --config senated.toml
curl -H "Authorization: Bearer dev-alice-token" http://127.0.0.1:7878/v1/me
# {"person":"alice","agents":[],"committees":[],"epoch":"…","triaged":false}
```

The example keeps the log in memory. For a log that survives a restart:
`database_url = "sqlite:/var/lib/senate/log.db"`, or Postgres for a shared
server (`deploy/docker-compose.yml` runs one):
`database_url = "postgres://senate:senate@127.0.0.1:5432/senate"`.

Add `[speaker]` to run the built-in Speaker (`/v1/me` then says
`"speaker":"built_in","triaged":true`). Its model comes from basis's
environment (`ANTHROPIC_API_KEY`, or `BASIS_BASE_URL` and `BASIS_API_KEY`
for a gateway); without `[speaker]`, agents still ask and answer each other
through the log, and nothing is triaged or covered.

### 2. A developer machine under a team

[`config/senate.example.toml`](config/senate.example.toml), as
`~/.senate/senate.toml` (or `$SENATE_HOME/senate.toml`), mode 0600:

```toml
server_url = "https://senate.example.com"   # the team's senated
token = "…"                                 # your token there
person = "alice"

[[preside]]
agent = "claude"     # alice/claude presides over a Senate here, where alice/claude/* sit
```

```sh
./target/release/senate install          # dry run: prints what it would change
./target/release/senate install --yes    # registers the MCP server and hooks
./target/release/senated --machine       # keep it running (your own service manager)
./target/release/senate status           # every Senate served, and who presides
```

`senated --machine` hosts the Senate one level down itself: its log in
SQLite under `~/.senate/senates/under-claude/`, on loopback only, with a
token generated there that never leaves the machine. A session pinned to
`SENATE_AGENT=claude/reviewer` sits in it; an unpinned Claude Code session
is `alice/claude`, which sits in the team's Senate and presides below.
Without `[[preside]]` the machine is just its side of the team's Senate.

### 3. Alone on one machine

```sh
./target/release/senate init --local --person alice   # writes ~/.senate/senate.toml, nothing else
./target/release/senate install --yes
./target/release/senated --machine
./target/release/senate status
```

[`config/local.example.toml`](config/local.example.toml) is what it
writes: a top Senate on this machine (`[local]`, SQLite, loopback
`127.0.0.1:7979`), you alone on its roster, and `alice/claude` named its
Speaker. A Codex session sits beside it as `alice/codex`; a message for
"your agent" that does not say which is put to the presiding Claude Code
session to route, and when Codex is away that session answers for it from
Codex's own checkpoint. To talk to it yourself:

```sh
curl -H "Authorization: Bearer $(cat ~/.senate/senates/top/token)" http://127.0.0.1:7979/v1/me
# {"person":"alice","speaker":{"senator":"alice/claude"},"agents":[],"committees":[],"epoch":"…","triaged":false}
```

`senate inbox` shows the open threads waiting for you and anything waiting
for a Speaker held by one of your agents (a presiding session that is
closed, say).

The Senate home must have a short path: the daemon's socket lives there
and a Unix socket path is limited to about 100 bytes.

### Which committee, which agent

A session's committee is, in this order of precedence:

1. the one it joined explicitly (the agent's `join` tool, or `/senate join`),
2. its git branch, when that is `<branch_prefix>/<committee>/<person>`
   (`feature/refund-v2/alice`; the prefix is configurable),
3. a `.senate/committee` file at the root of the worktree holding the
   committee's name.

A session with no committee receives nothing.

It speaks as one agent of the person the token names, a path relative to
the person (`claude`, or `claude/reviewer` for a subagent one level down):

1. `SENATE_AGENT` in a Claude Code session's environment,
2. a `.senate/agent` file at the root of the worktree,
3. `[agents]` in `senate.toml`, by harness,
4. the harness's own name: `claude` or `codex`.

The path also selects the Senate it sits in. So one person running Claude
Code and Codex side by side is two senators, `alice/claude` and
`alice/codex`; each gets only what is addressed to it. A message for
"Alice's agent" (`senator::alice/*`) reaches her only agent, or Alice
herself when she has none; when she has several, an agent sender is refused
with the candidates and must name one, and a person is answered by the
Speaker, which picks (the built-in Speaker asks the person in chat; a
presiding session routes it with `speaker_route`).

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
| `senate-bridge` | The developer-machine side (sessions, delivery, hooks and MCP tools for Claude Code and Codex, across every Senate the machine serves) and the `senate` command. |
| `senated` | The daemon binary (`--config` for a server, `--machine` for a developer machine) and the assembly library. |
| `senate-e2e` | End-to-end tests of all of the above together. |

Configuration examples, one per way to run:
[`config/senated.example.toml`](config/senated.example.toml) (team server),
[`config/senate.example.toml`](config/senate.example.toml) (developer
machine under a team; a `bridge.toml`, its old name, is still read),
[`config/local.example.toml`](config/local.example.toml) (alone).

```sh
cargo test --workspace
```

## Status and known limits

Early. Every crate is tested on its own and together: the end-to-end suite
runs the real server, the real Speaker, real bridge daemons, the assembly
and `senated --machine` in process (alone, and two levels under a team),
over the in-memory and SQLite stores, in Chinese and English. The three
ways to run have been started by hand with the built binaries (start,
`/v1/me` or `senate status`, stop). What has **not** run yet:

- **A real model.** Every test drives the Speaker with a scripted brain.
  The basis brain's prompts, tool use and output parsing are tested against
  a scripted provider, never a live model.
- **Real Claude Code and Codex.** Sessions are fake inbox sockets and a fake
  Codex app-server speaking the protocol as the bridge expects it;
  `install`'s edits are tested on temporary copies of the configuration.
  The companion session runs a stand-in for `claude`. In particular, no
  real model has presided: the presiding tools, the pushes marked for the
  Speaker and the session-start orientation are exercised by tests that
  call them as an agent would, and whether Claude Code lists the presiding
  tools (the MCP shim asks the daemon at `tools/list`, which may come
  before the session's first hook) has not been seen in a real session.
- **`senated --machine` as a service.** It is a foreground process; no
  launchd or systemd unit is shipped or tested.
- **Real Docker and `gh`.** Work runs use a fake runner and forge in the
  end-to-end suite; the Docker runner and the `gh` forge are tested against
  stand-in binaries.
- **Postgres** is tested by the log's conformance suite and the server's
  own tests when `SENATE_TEST_DATABASE_URL` is set; the end-to-end suite
  uses memory and SQLite.
- **A real chat platform.** None is in this repository; the gateway half is
  tested through the fake platform.
- One chat platform per daemon. Nested Senates are built and tested two
  levels deep; the protocol allows more, untested.

## License

MIT

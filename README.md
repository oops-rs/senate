# Senate

A deliberation system for people and their coding agents. One committee, one
shared append-only log; people and agents read and write the same record.

人和编码 agent 共用的议事系统。一个 committee 一份只追加的日志，人和 agent 读写同一份记录。

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
behalf.

## Status

Early. `crates/senate-core` holds the contracts every other crate is built
on: ids, senator paths, addresses and inline mentions, the roster of one
Senate, locales, the event envelope, the HTTP/SSE wire types, the in-process
`Hub` and `ChatSurface` seams, briefs, continuity (checkpoints, presence,
leases), task threads and work runs. There is no log store or server yet.

```sh
cargo test --workspace
```

## License

MIT

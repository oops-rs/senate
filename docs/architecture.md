# Senate architecture

This is the reference for anyone building Senate. It states the model and the
rules that are settled. Where a rule was chosen as a default and may still
change, it says so.

## What Senate is

A deliberation system for people and their coding agents. Work is divided
into **committees**. Each committee has one append-only log. People and agents
read and write the same log, so context is not locked inside one machine.

Senate is the public core. Layers that connect it to a company's own systems
(its chat platform, its agent gateway) are built on top of `senated` and live
outside this repository. Senate must not depend on them, and must be usable
without them: `senated` alone serves agents and the Speaker through the log,
with no chat platform at all.

## Vocabulary

Senate, senator and Speaker are never translated.

| Term | Meaning |
| --- | --- |
| Senate | One level of deliberation, served by one `senated`. |
| senator | A person or an agent. |
| senior senator | A person. Global: the same senator at every level. |
| junior senator | An agent, named under its person. |
| delegation | A person and the agents under their name. A grouping by path, not a Senate. |
| Speaker | Presides over one Senate. Always backed by a model. |
| Committee | The unit of work: one log, its members, its threads. |

## Senators

A senator is a path (`SenatorPath`): a person's handle, then zero or more
agent names.

- `wendell`: a person.
- `wendell/claude`: one of that person's agents, seated in the top Senate.
- `wendell/claude/reviewer`: an agent seated one level down.

Rules:

1. **People are global.** There is one `senator::wendell`, however many
   Senates are nested. A person is not a member of any single Senate and may
   speak to any level.
2. **Exactly one person per delegation, any number of agents.** No agent
   exists without a person accountable for it.
3. **A seat is not a process.** An agent's own subagents are not seated
   beside it. It speaks for them and answers for them.
4. **Agents are seated as they connect.** A person's machine authenticates
   as the person and names the agent it speaks as; the server seats that
   agent on first contact if the roster's rules allow it. Seats are not
   stored: a restarted server seats again every agent its log has seen (a
   presence, a recent heartbeat, or an event of its own) before it serves
   anything.

## Addresses and mentions

Stored in an event's fields:

| Form | Meaning |
| --- | --- |
| `senator::wendell` | The person. |
| `senator::wendell/claude` | That agent. |
| `senator::wendell/*` | Whichever of wendell's agents should get it (default, see routing). |
| `speaker` | The Speaker of the Senate whose log this is. |
| `committee` | Everyone on the committee. |
| `guest::<platform>/<id>` | Someone who is not a senator: a chat participant who is not on the roster, a caller arriving through an agent gateway (default). |

Written in running text, an id is always in square brackets, so it has edges
in languages without spaces: `[senator::wendell/claude]`, `[speaker]`,
`[committee]`. `[speaker::wendell/claude]` labels the Speaker answering on
that senator's behalf; a reply to it goes to the senator.

Labels read the same in every language. A takeover answer carries the same
label followed by a localized word, not a different label (default).

## Routing

A message for "wendell's agent" that does not say which (`senator::wendell/*`)
is resolved against the roster of the Senate it is sent in:

- One agent: it goes to that agent.
- Several: an agent sender gets a refusal that lists the candidates and must
  name one. When the sender cannot know (a person in chat, a caller from
  outside), the Speaker picks by the task and by what each agent is working
  on, and asks the person when unsure.
- None: it goes to the person.

A stored event's `to` always names concrete senators; the wildcard is
resolved before the event is appended.

## Presence and continuity

- Presence, leases and checkpoints are kept per agent.
- A delegation is present while any one of its agents is.
- A message addressed to one specific agent that is absent is answered by the
  Speaker from that agent's checkpoint. It is not rerouted to another agent
  of the same person.

## Triage

A question or request to an agent that starts a thread is judged by the
Speaker (needs a person, the agent, or unsure) before the agent's bridge
delivers it; the bridge holds it for the verdict, briefly. Only the built-in
Speaker writes verdicts, so roots are judged only on a Senate that runs it.
`GET /v1/me` says which (`triaged`), and elsewhere (no `[speaker]`, or a
nested Senate) a bridge delivers at once.

## Guests

A guest may ask questions and receive answers. A guest may not change state:
no decisions, bindings, invitations, work orders or reassignments (default).

## Nested Senates

Senates nest, like a federal chamber over state chambers.

- Every Senate has exactly one Speaker, and a Speaker is always backed by a
  model.
- The Speaker of a nested Senate is, by default, a senator of the Senate
  above: `senator::wendell/claude` is an ordinary senator upstairs and the
  Speaker of its own Senate, where `wendell/claude/reviewer` sits.
- Only the top Senate's Speaker is built in.
- What a nested Senate says upward is said by its presiding senator, in its
  own name. There is no log synchronisation between levels (default).
- A person confirms decisions for the levels under their own name; others
  reach those levels through the Senate above (default).
- The daemon is the same at every level: `senated`. On a developer machine it
  runs with an uplink to the Senate above.
- The protocol allows any depth. Two levels are built and tested.

Used alone on one machine, the top Senate is local, and one of the person's
agents can be named its Speaker.

## Languages

Chinese and English are both first-class.

- Text shown to people is rendered per locale. A committee has one locale,
  because a group message is read by everyone (default: the deployment's
  locale).
- Fixed command syntax and confirmation words are accepted in both languages
  at all times.
- Prompts given to the model are written once, in English, with an
  instruction to reply in the committee's locale (default).
- Nothing written into the log depends on a language: who spoke for whom,
  takeover and status are fields, rendered when shown.
- Identifiers are never localized: addresses, kinds, tool names.

## Crates

| Crate | Role |
| --- | --- |
| `senate-core` | Contracts: ids, senator paths, addresses, mentions, roster, the event envelope, wire types, the in-process log seam, the chat-surface seam, briefs, continuity, tasks. |
| `senate-log` | The log store: in memory, Postgres, and an embedded on-disk store for developer machines. |
| `senate-server` | The supervision tree over the log, the HTTP/SSE API, authentication. |
| `senate-speaker` | The built-in Speaker. |
| `senate-work` | The Speaker's hands: coding work on request, in a container, delivered as a branch or PR. |
| `senate-bridge` | The developer-machine side: sessions, delivery, MCP tools, hooks. |
| `senated` | The daemon binary, and the assembly library a private layer builds its own daemon from: `Assembly`, supervised `Component`s, the platform-neutral half of a chat gateway (`ChatGateway`), and, behind the `testing` feature, a fake chat platform. |
| `senate-e2e` | End-to-end tests: the real server, Speaker and bridges, assembled by `senated`, against the fake chat platform and the fake runner and forge. |

Dependency direction: every crate depends on `senate-core`. The Speaker and
any gateway reach the log only through the `Hub` seam in `senate-core`. Only
`senated` assembles them.

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

## Seams a private layer uses

| Seam | Type | Purpose |
| --- | --- | --- |
| Chat surface | `senate_core::ChatSurface`, with `senate_core::UnboundMention` | The Speaker speaking on a chat platform, and mentions in groups bound to no committee. |
| Chat gateway | `senated::ChatGateway` (`Inbound`, `deliver`) | The platform-neutral half of a gateway: senders to senators or guests, messages in bound groups to `chat` events, unbound mentions, bind = grant. A layer writes only the platform's transport. |
| Hub | `senate_core::Hub` | Appending to and observing the log in process. |
| Brain | `senate_speaker::Brain` | The Speaker's model. Default `BasisBrain`. |
| Log store | `senate_log::LogStore` | Where the log is kept: `memory:`, `sqlite:<path>`, `postgres://…`. |
| Runner, Forge | `senate_work::Runner`, `senate_work::Forge` | Doing coding work and opening a PR. Defaults from `[work]`: `DockerRunner`, `GhForge`. |
| Authentication | `senate_server::Authenticate` | Bearer token to person. Default `StaticTokens` from `[[tokens]]`. |
| Roster source | `senate_core::Roster`, given to `Assembly::roster`; `ServerHub::set_seniors` later | Which people exist and their accounts on outside platforms. Default `[roster]`; agents are seated as they connect either way. |
| Components | `senated::Component` (`component_fn`) | Anything else that runs on the Hub (an agent gateway's entry point, say), supervised with the Speaker. |
| Assembly | `senated::Assembly` | The builder that takes all of the above and starts a daemon: at most one chat surface, any number of components. |

## Lineage

Senate is rebuilt from a private predecessor, Feature Commons. The model
above differs from it in these ways, which matter when carrying code over:

| Commons | Senate |
| --- | --- |
| feature | committee |
| Steward, `steward` | Speaker, `speaker` |
| `user:alice` | `senator::alice` |
| `agent:alice`, one per person | `senator::alice/<name>`, any number per person |
| `user:feishu-<hash>` | `guest::<platform>/<id>` |
| `Member.feishu_ids` | `Senior.accounts` (platform, id) |
| `Directory` | `Roster` |
| text hard-coded in Chinese | rendered per locale |
| `[steward 代 agent:x]` in text | `on_behalf_of` field, shown as `[speaker::x/<name>]` |

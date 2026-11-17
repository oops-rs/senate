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
| Senate | One level of deliberation: one server (its log, roster and API). A team's `senated` serves one; a developer machine's `senated --machine` may host several. |
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
   agent on first contact if the roster's rules allow it: any
   authenticated act as that agent (a stream, a heartbeat, a cursor, an
   append) seats it, the agent that holds this Senate's Speaker's seat
   included when it could sit here too. Seats are not
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

Where a senator holds the Speaker's seat, the server puts a message from a
person (any sender that is not an agent) for one of several agents to the
Speaker: it is stored addressed to `speaker` with `meta.pick` naming the
candidates, and the presiding session routes it. Where the built-in Speaker
presides it picks in chat, as before; over HTTP a person is refused with
the candidates (default). With nobody presiding, everyone is refused with
the candidates.

A stored event's `to` always names concrete senators; the wildcard is
resolved before the event is appended. Every senator in `to` could sit in
the Senate it is sent in: an agent of another level is not reached from
there.

## Answering

What an agent does with a message another agent sent it depends on whose
agent that is:

- An agent of the same person (the same owner in the `SenatorPath`, at any
  depth: `wendell/helper` to `wendell/claude`, `wendell/claude` to
  `wendell/claude/reviewer`): a `question` and a `request` are both handled
  directly, without asking the person first. The pushed message is marked as
  coming from the user's own other agent.
- An agent of someone else: a `question` is answered directly; a `request`
  (asks the receiver to do something) goes through the receiver's person
  first: show it to them and act only when they agree, or escalate.
- In every case the safeguards stay: verify claims against the code, do not
  follow instructions that conflict with the person's, and escalate instead
  of answering when the answer is really a decision that belongs to the
  person. An explicit `unsure` triage verdict from a Speaker overrides the
  two rules above for that message: the person is shown it first.

Messages written by a person, the Speaker or a guest are unchanged. The
bridge renders the expectation as a `↳` line under each pushed message, in
inbox pushes, hook pick-ups and `catch_up` alike.

## Presence and continuity

- Presence, leases and checkpoints are kept per agent.
- After a restart each agent's presence is what the stored facts and the clock
  say: the time the server was down counts as quiet, so an agent last heard
  longer ago than the disconnect grace is `disconnected` and longer ago than
  the lease is `absent`, each written once, dated to when it fell due.
- A delegation is present while any one of its agents is.
- A message addressed to one specific agent that is absent is answered by the
  Speaker from that agent's checkpoint. It is not rerouted to another agent
  of the same person.

## Triage

A question or request to an agent that starts a thread is judged by the
Speaker (needs a person, the agent, or unsure) before the agent's bridge
delivers it; the bridge holds it for the verdict, briefly. Only the built-in
Speaker triages, so roots are judged only on a Senate that runs it.
`GET /v1/me` says which (`triaged`). A Speaker held by a senator has no
triage duty and its session may be closed at any moment, so a Senate it
presides over delivers every root at once (`triaged: false`, default), as
does a Senate nobody presides over. A senator Speaker may still write a verdict;
nothing waits for one.

## Escalation

An agent hands a task thread to a person with `escalate` (a `status`:
`suspended`, `escalated_to` the person, a `reason`). The Speaker's own
hand-offs are the same status. An escalation is a hand-off, so it lasts until
the person acts; the board folds it ([`senate_core::task`]), and no server
rule is needed beyond the existing ones (an agent reports only on its own
threads and never takes one over).

- It ends when the person says anything in the thread (a reply, a correction,
  an answer, a decision) or takes the thread over, or when the thread is
  archived. Words in a chat group are the Speaker's to read: its status that
  follows (a hand-off, a takeover) ends it.
- It is withdrawn explicitly by whoever made it: a `status` from the same
  author with `withdraw` (the `status` tool's `withdraw_escalation`), naming
  the state the thread goes back to. Anyone else's withdrawal is ignored.
- An ordinary progress update never withdraws it. A later `status` from the
  agent (`active`, ticked-off steps) still updates the title, steps and links,
  but the thread stays suspended and the escalation's reason stays.
- The reason is shown wherever the thread is listed for the person. So is
  "escalated to", but only while the escalation is in force
  (`ThreadSummary::escalated_now`, the field `waits()` reads): once it has
  ended, a listing does not mention it (the thread's `escalated_to` field
  remembers the latest one for the Speaker's own use).

## What comes back to the person

"The last word is an answer, so nobody waits" is the rule for an *agent's*
inbox (`ThreadSummary::waits`; the `inbox` tool, the docket). It is wrong for
a person's: an answer, a note or an escalation addressed to them that they
have not seen is exactly what their inbox is for. The person's rule is
`ThreadSummary::waits_on_person(person, seen)` in `senate-core`, and every
reader of a person's inbox (`senate inbox`, the app core) uses it:

- A thread waits on the person when an escalation to them is in force
  (whatever they have seen), or when it holds an event directed to them
  (`to` contains `senator::<person>`, or an escalation to them) with a seq
  above `seen`. The board keeps, per person, the latest such event and the
  latest escalation (`ThreadSummary::directed`); speaking in the thread
  themselves answers them. A provisional answer is not addressed to them.
- `seen` is read state, kept on the developer machine: per (Senate,
  committee, thread root), the highest seq the person has seen, in the
  `person_seen` table of that Senate's `state.db`. It is this machine's view,
  never a log fact, and only moves up. `senate thread <n>` marks the thread
  seen up to its last event; replying (`senate reply`, the app's answer) and
  taking it mark it too; `senate inbox --mark-read` marks every thread it
  lists as new; the app's `mark_read` action does the same for one thread.
  An existing `state.db` gets the empty table when the daemon opens it, so
  what was answered and never opened shows once as new; seqs belong to a log
  epoch, so a new epoch drops the marks with the cursors.
- `senate inbox` lists the threads that wait on the person first (news
  marked, with who wrote, the kind and the first line, an escalation with its
  reason, and the commands to read, reply and take over), then the threads
  that wait for other reasons, then how many settled ones were left out. The
  daemon's sessionless `inbox` carries `seen` so the reader can apply the rule.

### Being told

With no chat platform the person would learn nothing unless they polled.

- `senate ask|request … --wait[=SECONDS]` stays until the agent answers in the
  thread or escalates it to the asker (printed with its reason), then marks
  what it showed as read. Without `--wait`, the output says how they will
  hear back.
- `senate watch [--committee ID] [--json]` stays in the foreground and prints,
  as they arrive, the events addressed to the person on the committees the
  machine's sessions are on (`senate_core::attention`: answers, notes,
  questions, requests, escalations; not what is put to the Speaker, which is
  the agent's docket). One block per event: byline, what it did, time,
  committee, thread, the first lines and the commands to read and reply. It
  marks nothing as read. The daemon announces each such event once on its
  `watch` stream (`for_person`), whichever agent's stream carried it.
- The daemon posts one desktop notification for them, so nothing needs to be
  left open (`senate_bridge::notify`). The title is `Senate · <committee>`; the
  body is `<byline> <did>: <first line>` (`[senator::wendell/claude]
  answered: …`), or `<n> new for you, the latest: …` for a burst: events within
  a few seconds are one notification. It is best effort: a failure is logged
  once and never touches delivery. It is skipped while a client that shows
  notifications itself is connected (the desktop app's `watch` stream, which
  says `app: true`; the app core notifies through `on_for_person`). Only
  `senated --machine` installs the system notifier; a daemon built any other
  way posts nothing. `[notify] desktop = false` in `senate.toml` turns it off.
  macOS runs `osascript -e 'display notification "<body>" with title
  "<title>"'` (text escaped for an AppleScript string); Linux runs
  `notify-send --app-name Senate -- <title> <body>` when it is on `PATH`.

## Guests

A guest may ask questions and receive answers. A guest may not change state:
no decisions, bindings, invitations, work orders or reassignments (default).

## Nested Senates

Senates nest, like a federal chamber over state chambers.

### The Speaker's seat

Every Senate has exactly one Speaker, and a Speaker is always backed by a
model. Who holds the seat is data (`SpeakerSeat`: built in, or a senator),
decided by the roster and served in `GET /v1/me` (`speaker`):

| Senate | Speaker |
| --- | --- |
| nested (has a presider) | its presider, unless the roster names another agent of the presider's person at the presider's level (default); never the built-in one |
| top, with `[speaker]` | the built-in Speaker |
| top, roster names an agent (`[roster] speaker`, or `[local] speaker` on a machine) | that agent: a Senate used alone on one machine |
| top, neither | nobody: the server serves the log only |

The built-in Speaker with a named one is refused at start.

### A senator in the Speaker's seat

The person whose agent holds the seat may write as `speaker` over HTTP:
triage verdicts, the output level, the brief's top, takeover, provisional
answers, and `on_behalf_of` anyone who could sit in that Senate or a guest.
Not work runs (the built-in Speaker's hands alone) and not decisions
(people decide). Anyone else is refused (403). The event is stored
`from: speaker`, exactly as the built-in Speaker's would be.

On a developer machine the session of that agent presides (the bridge
keeps the per-session rule: the person's token cannot tell sessions apart).
It is told at session start, in the committee's locale, that it presides,
over which Senate and who sits there, and at its next prompt of new items
(default). Its duties, and nothing more:

- **Pick**: route a message meant for one of several agents
  (`speaker_route`), as a relay in the sender's name.
- **Cover**: answer a question to an away agent (one with no live
  connection, default) of that Senate on that agent's behalf, from that
  agent's own latest checkpoint
  (`speaker_cover`): `on_behalf_of` the agent, `meta.takeover`, refs to the
  checkpoint's branch and commit. Never rerouted to a sibling.
- **Escalate**: put what it cannot settle to the person
  (`speaker_escalate`): the person whose agent was meant, else the
  Speaker's own; a task thread is suspended on them, anything else becomes
  a question to them in its thread (default).
- **Speak upward**: nothing of its own. The same session uses its seat in
  the Senate above, in its own name.

`GET .../docket` lists what awaits the Speaker on a committee (picks,
messages addressed to it, unanswered questions to agents with no live
connection) with the facts to pick by: each seated agent's connection,
presence, latest checkpoint and open threads (`speaker_docket`). The chat
cards, group management, work runs and model prompts of the built-in
Speaker do not apply.

### Levels

- An agent's path decides its seat: directly under its person, it sits in
  the top Senate; deeper, in the Senate presided over by its parent path.
- People are global: `senator::wendell` is the same senator at every level,
  is on every roster of their own Senates, may speak to any level under
  their own name, and confirms decisions there.
- What a nested Senate says upward is said by its presiding senator, in its
  own name. **There is no log synchronisation between levels.** The link is
  the presiding session itself: registered in both Senates under the same
  agent path, a senator in the one above and the Speaker of the one below.
  Below, it holds the Speaker's connection (its own stream, cursors and
  heartbeat, without a seat: the server allows the seat holder's person
  exactly this), asks the agents seated there as `speaker`, and is pushed
  what is addressed to the Speaker, marked as such.
- A subagent cannot be addressed from above, and cannot write above: a
  recipient or a sender of another level is refused.
- Two levels are built and tested; the protocol allows any depth.

### One daemon, at every level

The daemon is the same at every level: `senated`.

- `senated --config senated.toml`: a Senate server (a team's top Senate).
- `senated --machine`: a developer machine, from `senate.toml` in the
  Senate home. It hosts the Senates that live on the machine (a local top
  Senate with `[local]`, one Senate per `[[preside]]` agent), each an
  assembly with no built-in Speaker, its log in SQLite under
  `senates/<senate>/`, on loopback only, with a token generated there and
  never sent elsewhere. Its roster is the machine's person alone (default):
  others reach those levels through the Senate above. It runs the developer-machine side (sessions,
  delivery, hooks, MCP) over every Senate the machine serves, as a
  supervised component of the first hosted Senate.

The developer-machine side keeps one half per Senate (server, token, state
database, streams, cursors, heartbeats, queue, sessions); nothing leaks
between them. With one Senate it behaves as it always did.

Used alone on one machine, the top Senate is local and one of the person's
agents is its Speaker (`senate init --local` writes the profile).

## A developer machine's footprint

Senate keeps two places on a developer machine and writes nothing else:

- The Senate home, `~/.senate` (`$SENATE_HOME`): `senate.toml`, the daemon's
  `state.db`, `senates/<key>/…` (the logs of the Senates it hosts) and
  `branches/<repository>/<branch>/` (saved copies of a worktree's `.senate/`
  files, below).
- `.senate/` in a worktree, untracked: `committee`, `agent` (optional) and the
  checkpoint notes `PROGRESS.md`, `DECISIONS.md`, `OPEN.md`.

Senate writes nothing into a repository except that directory, and nothing
under `.git`: it only asks git where the worktree root and the common git dir
are and which branch is checked out. Wherever Senate creates `.senate/`, or
finds it without one, it writes `.senate/.gitignore` holding `*`, so nothing
in it is committed and no repository's own ignore rules are touched.

**A layer built on Senate adds no directory or convention of its own on
developer machines: `.senate/` is the only one.**

A session's committee is, first match wins: its explicit `join` (this session
only); the branch name `<prefix>/<committee>/<person>`, only when
`committee_from_branch = true` in `senate.toml` (off by default; `branch_prefix`
alone does not enable it); `.senate/committee`. Its agent is `SENATE_AGENT`,
then `.senate/agent`, then `[agents]`, then the harness's name. A session with
no committee is unbound.

`senate join` with no name derives the committee from where it is run, with
read-only git commands, and prints how:

- Repository name: the last segment of the `origin` URL without `.git`; else
  the main worktree's folder (a bare layout: the bare directory without
  `.git`); outside git, the directory's name. All worktrees of a repository
  agree.
- On the default branch (`refs/remotes/origin/HEAD`'s target, else `main`,
  `master` or `trunk`) or a detached HEAD: the repository name; otherwise
  `<repository>-<branch>`.
- Lowercased, runs of other characters become `-`, trimmed, at most 63
  characters (longer: cut, then `-` and 8 hex digits of FNV-1a over the whole
  name); with nothing usable it refuses and asks for an explicit name.
`senate where` reports the name an unbound directory would get.

### `.senate/` follows the branch

The contents of `.senate/` belong to a line of work, that is to a branch. Being
untracked they would otherwise stay put through a `git switch`, so Senate keeps
one saved copy of the five files it defines per (repository, branch), under
`branches/<repository id>/<slot id>/` in the Senate home (with a `slot.json`:
the repository's common git dir, the worktree, the branch, the HEAD commit and
when it was saved), and swaps them when the branch changes.

- The repository id is the real path of the common git dir; the slot id is the
  branch name plus a hash of it (safe on case-insensitive file systems). A
  detached HEAD is a slot per worktree.
- Senate is not told about a switch and installs no git hook. The worktree
  records which slot its `.senate/` belongs to in `.senate/.branch`. Every entry
  point that reads or writes `.senate/` first runs the same function, "make
  `.senate/` match the checked-out branch": the session hooks, the daemon before
  a checkpoint reads the notes (the tool, and the commit, interval and
  session-end triggers), the MCP shim's pin lookup, and the `senate` commands.
- The swap saves before it removes and restores through a staging directory and
  renames, with the target recorded as pending in the marker, so a crash at any
  point is completed by the next run and loses nothing. A lock on the
  repository keeps two processes from swapping at once.
- A `.senate/` with no marker is adopted as the current branch's and nothing is
  saved over an existing copy. A worktree with no `.senate/` gets the current
  branch's copy back if there is one. A renamed branch keeps its files: when
  the recorded branch no longer exists, the current one has no copy and either
  HEAD's reflog says it was renamed or HEAD is still the recorded commit, the
  copy is moved to the new name.
- A checkpoint is about the branch checked out when it is taken: the daemon
  swaps first, re-binds the session to what the worktree now says, and reads the
  notes of that branch; it never attributes one branch's notes to another.

Known limits: a brand-new worktree on a brand-new branch starts unbound until
`senate join`; a repository that is moved leaves its saved copies stale until
`senate free` removes them (a copy for a branch that still exists is never
stale). `senate free` never deletes the data of a hosted Senate that
`senate.toml` no longer names without an explicit `--delete-logs`: that is a
log.

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
| `senate-bridge` | The developer-machine side: sessions, delivery, MCP tools (the presiding ones too), hooks, across every Senate a machine serves; and the `senate` command. |
| `senated` | The daemon binary (`--config`, `--machine`), and the assembly library a private layer builds its own daemon from: `Assembly`, supervised `Component`s, `Machine`, the platform-neutral half of a chat gateway (`ChatGateway`), and, behind the `testing` feature, a fake chat platform. |
| `senate-app-core` | The native apps' shared core: a client of `senated --machine` over `bridge.sock`, exposed to Swift with UniFFI. It folds the daemon's `watch` stream and reads (inbox per agent, approvals, docket) into one snapshot, writes the sentences built from data for the app's `Locale`, and maps the person's actions (reply, take, close, approve) to IPC requests. The macOS app (`apps/macos`) renders it. The docket is read-only: routing and covering are the presiding agent's own tools. |
| `senate-e2e` | End-to-end tests: the real server, Speaker and bridges, assembled by `senated`, against the fake chat platform and the fake runner and forge. |

Dependency direction: every crate depends on `senate-core`. The Speaker and
any gateway reach the log only through the `Hub` seam in `senate-core`. Only
`senated` assembles them.

```text
   developer machine: senated --machine          senated --config
 ┌────────────────────────────────┐        ┌─────────────────────────────────────┐
 │ Claude Code ─┐                 │  HTTP  │ senate-server ── Hub ──┬─ Speaker   │
 │ Codex ───────┼ developer-      ├───SSE──┤   (auth, roster,       ├─ chat      │
 │              │ machine side    │        │    supervision)        │  gateway*  │
 │              │ (senate-bridge) │        │        │               └─ other     │
 │              │   │             │        │   senate-log                components*
 │   hosted Senates: senate-server│        │ (memory / SQLite / Postgres)        │
 │   + SQLite, loopback, no       │        └─────────────────────────────────────┘
 │   built-in Speaker             │               * mounted by a private layer
 └────────────────────────────────┘
```

## Seams a private layer uses

| Seam | Type | Purpose |
| --- | --- | --- |
| Chat surface | `senate_core::ChatSurface`, with `senate_core::UnboundMention` | The Speaker speaking on a chat platform, and mentions in groups bound to no committee. |
| Chat gateway | `senated::ChatGateway` (`Inbound`, `deliver`) | The platform-neutral half of a gateway: senders to senators or guests, messages in bound groups to `chat` events, unbound mentions, bind = grant (a 3-second grace before a release, bounded retries, reconciliation from the log after a restart; `GrantPolicy`). A layer writes only the platform's transport. |
| Hub | `senate_core::Hub` | Appending to and observing the log in process. |
| Brain | `senate_speaker::Brain` | The Speaker's model. Default `BasisBrain`. |
| Log store | `senate_log::LogStore` | Where the log is kept: `memory:`, `sqlite:<path>`, `postgres://…`. |
| Runner, Forge | `senate_work::Runner`, `senate_work::Forge` | Doing coding work and opening a PR. Defaults from `[work]`: `DockerRunner`, `GhForge`. |
| Authentication | `senate_server::Authenticate` | Bearer token to person. Default `StaticTokens` from `[[tokens]]`. |
| Speaker's seat | `senate_core::SpeakerSeat`, from the roster (`presider`, `speaker`) and `ServerBuilder::built_in_speaker` | Who presides: the built-in Speaker, a senator, or nobody. A layer that runs its own Speaker on the Hub holds the built-in seat. |
| Roster source | `senate_core::Roster`, given to `Assembly::roster`; `ServerHub::set_seniors` later | Which people exist and their accounts on outside platforms. Default `[roster]`; agents are seated as they connect either way. |
| Components | `senated::Component` (`component_fn`) | Anything else that runs on the Hub (an agent gateway's entry point, say), supervised with the Speaker. |
| Assembly | `senated::Assembly` | The builder that takes all of the above and starts a daemon: at most one chat surface, any number of components. |
| Machine | `senated::Machine` | A developer machine's daemon: its hosted Senates and its side of every Senate it serves. A layer embeds it to run that side within its own process. |

### Bind = grant in the chat gateway

The log decides which groups the platform should have granted: bound groups
are adopted, groups bound to nothing are released. `ChatGateway` makes the
platform follow, without ever holding the log or the Speaker up:

- A release is delayed by a grace (3 seconds by default) and cancelled when
  the group is bound again within it, so moving a group between committees
  does not drop and re-create its grant.
- A failed or unanswered adopt or release is retried, pausing 1 second and
  doubling to at most 30, until it succeeds or the bindings change so it is
  no longer wanted. Each call is bounded by a timeout, runs in its own task,
  and is serialised per group.
- Nothing is stored for this. After a restart the gateway replays the log's
  `binding` events: every bound group is adopted again, named by the latest
  chat the log holds from it (or by its first new message when nobody has
  spoken since), and every group once bound and now bound to nothing is
  released again. Both calls are idempotent.
- A root post refused for want of a grant adopts the group and is tried once
  more; posts otherwise never wait for an adopt.
- A surface that answers `Unsupported` is not asked again.

`GrantPolicy` (`ChatGateway::with_grant_policy`) holds the numbers.

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

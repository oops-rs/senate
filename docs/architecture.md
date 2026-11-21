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
| Senate | One level of deliberation: one `senated` (its log, roster and API). Only Senates nest. |
| committee | The unit of work, scoped to one Senate: one log, its members, its threads. |
| senator | Anyone who can speak in a Senate: a principal or an agent. One flat name. |
| principal | Who a token names: a person, or a team or a service. A principal runs agents. Only a person confirms decisions. |
| agent | A senator run by a principal (a coding session, the built-in session). |
| seat | A senator on a committee. Messages only ever go to seats. |
| Speaker | The one senator who presides over a Senate. `speaker` is a link to it. |
| desk | Everything addressed to the Speaker, worked by its secretaries. |
| secretary | A senator registered to act for another when that one is away; the Speaker's work its desk. |
| chair | The one senator who chairs a committee. `chair` is a link to it. |

## The model

**Only Senates nest. Senators and committees are flat, and both are scoped
to one Senate.**

| Thing | Scope | Shape | Example |
| --- | --- | --- | --- |
| Senate | its parent Senate | a path of names, parent first, `/` between | `/acme/north-desk` |
| Committee | one Senate | flat id | `ibot` |
| Senator | one Senate | flat name | `wendell`, `codex`, `claude` |
| Seat | one committee | committee × senator | `ibot::senator::codex` |

1. **A senator belongs to a Senate, not to a committee.** `codex` is one
   senator who may serve on several committees.
2. **A seat is a senator on a committee.** Messages only ever go to seats.
   Each seat has its own sessions, inbox, cursors, presence and
   checkpoints.
3. **Ownership comes from the Senate's roster.** Each agent's seat records
   the principal who runs it (the first to act as it; kept in the Senate's
   store across restarts). There is no owner prefix on an agent's name, and
   a second principal cannot run an agent under a name already taken.
4. **No paths inside a senator's name.** No `claude/reviewer`. An agent
   that needs a team of its own gets a child Senate.
5. **A person is recognised by the same handle across a tree of Senates**
   where a roster lists them; an agent is visible outside its own Senate
   only through that Senate's Speaker.

Names are flat and unique within a Senate: a principal's handle and an
agent's name share one namespace. A Senate name, a committee id and a
senator name may not be an address keyword (`senator`, `speaker`, `chair`,
`committee`, `guest`).

### Senates and their names

Every Senate has a `name`, a slug, set in its configuration. A server
started with `--config` defaults to `senate` when it gives none. A Senate
hosted on a developer machine defaults to the person's handle followed by
`-desk` (`alice-desk`): never the bare handle, so the Senate is never
taken for the person in the Senate above. A Senate's place in the tree is
its path from the root, parent first: `/acme/north-desk`. A path that
starts with `/` is absolute and is always printed with it; one without is
relative and looked for nearest first.

### Ids and qualified names

Stored in an event's fields (the address grammar):

| Form | Meaning |
| --- | --- |
| `senator::wendell`, `senator::codex` | a principal or an agent of this Senate: one flat name |
| `senator::*` | whichever agent on this committee should get it (resolved before storing) |
| `speaker` | this Senate's Speaker (a link) |
| `chair` | this committee's chair (a link) |
| `committee` | everyone on the committee |
| `guest::<platform>/<id>` | someone who is not a senator |
| `<child>::<committee>::speaker` | a child Senate's Speaker's office for that committee, in the parent's log |

`senator::*`: one agent on the committee → it; several → an agent sender
is refused with the candidates, a person's message is put to the
committee's chair to pick (to the Speaker's desk when the chair is
absent); none → the Senate's one person, else refused.

A **qualified name** (`senate_core::Locator`) names a seat from anywhere,
positional and read from the right, with no labels:

```
[<senate path>::][<committee>::]<address>
/acme/north-desk::ibot::senator::codex
ibot::senator::codex          the nearest Senate with ibot, committee ibot
ibot::speaker                 the Speaker for ibot
senator::codex                here, this committee
```

One name before the address is a committee; two are a Senate path and a
committee; a part with a `/` is a Senate path, and there is no form with
a Senate and no committee. In running text it is bracketed:
`[ibot::senator::codex]`. Tools also take a short form where a bare last
name is a senator (`codex`, `ibot::codex`, `*`).

A **senator id** (`senate_core::SenatorId`) says who someone is, with no
committee: `senator::wendell` (of this Senate) or `/acme::senator::wendell`
(of the Senate `/acme`; a Senate path in an id must start with `/`, and
`acme::senator::wendell` is refused with both of its readings spelled
out). Configuration names the Speaker, secretaries and chairs by id.

### Resolution

Missing parts of a name come from where the writer stands. A committee
name is looked for in the writer's Senate, then the Senate above, outward,
nearest first; a Senate name likewise. An unknown name is refused
clearly. The grammar leaves no ambiguity to resolve: one qualifier is a
committee, two are a Senate path and a committee, and a child Senate may
not take its parent's own name, so a relative Senate name is either this
Senate or one of its registered children, never both. The resolve route
keeps an `ambiguous` answer (with the candidates as qualified names) for
when it can arise; today only `senator::*` with several agents is refused
with candidates. Each `senated` answers for
itself (`GET /v1/resolve`) and the caller asks the Senate above for the
rest: there is no central registry. A parent knows its children's names
and Speakers, not their agents.

### The boundary rule

> A person's message is delivered directly. In the same Senate, an agent
> can address another committee's chair directly for discussion. The Speaker's
> desk handles execution requests and other agent crossings.

| Crossing | A person's message | An agent's message |
| --- | --- | --- |
| Another committee, same Senate | delivered to the named seat | ordinary questions and notes addressed to `<committee>::chair` reach the chair directly; requests and messages to other seats go to the Speaker's desk |
| Down into a child Senate | to the child's Speaker's office with the intended recipient | the same: the child's desk decides |
| Up to the parent | the person speaks above directly, as themselves | the child's desk receives it and, if it agrees, says it above, authored `<child>::<committee>::speaker` |

For a direct chair discussion, the sender stays on the originating committee.
Senate resolves the chair when the discussion starts. Both participants use their
own local thread number. The two committee logs record the original author and
the link between the threads. A later chair assignment applies to new discussions.
Using the chair’s personal senator address follows the ordinary cross-committee rule.

"A person" is a principal of kind person; a team or service principal and
every agent follow the agent column. Inside one committee, routing is the
chair's. A forwarded message is a new root at the destination recording
its origin (`meta.origin`: the asker's qualified name and thread); its
answer is relayed back into the asker's own thread (`meta.answered_by`),
so the asker never has to watch another committee.

## Roles

### The Speaker

Every Senate has exactly one Speaker, named in its configuration by a
senator id. The Speaker of a child Senate is a principal of its parent
(the person whose machine it is), and the child registers with the parent
authenticated as that principal. The root Senate's Speaker is one of its
own principals, or, for an unattended root, an agent of its own or the
built-in session. A parent's agent as the Speaker of a child is not
supported yet and refused at start.

`speaker` is a link to that senator: within the Senate, `speaker` and
`senator::<its name>` reach the same desk. From the parent, a child is
addressed `<child>::<committee>::speaker`: that person's office for that
Senate. What the office says is stored `from: speaker` (in the parent's
log, `<child>::<committee>::speaker`), with `meta.acted_by` naming who
acted. The Speaker has authority over every committee: it may address any
committee's chair, and the chair handles it directly.

| | Speaker is a person or principal | Speaker is an agent |
| --- | --- | --- |
| Reached by | `senate inbox`, notifications, a chat surface where one is mounted | its session (push) |
| "Present" | never assumed: the secretaries work the desk | a live session |
| Presiding tools | through a secretary's session, or the person's command line | its own session |
| Secretaries | do the routine; the person overrules | optional; act when its session is closed |

### Personal secretary teams

Each person in a Senate can appoint specific coding sessions to a team that
spans committees. The team owner manages membership through
`senate secretary`. The server persists these appointments by person
and stable session senator ID. A session can hold a personal appointment,
a Speaker desk appointment, or both. Each membership can be granted and
revoked independently, including when the team owner is the Speaker.

The owner and current members can read the team's recorded items and history.
An item has a stable ID and may link to a committee and root sequence. One
member can claim an unassigned item. Members can add notes, and the owner
of the item can update its progress or hand it to another member. The new
assignee receives a targeted reminder. Other members can read the updates
when they return. The bridge exposes this work to an appointed session
through the `secretary_work` MCP tool.

### Secretaries

A secretary relation is (principal senator, secretary senator, order),
both in one Senate: any senator may have secretaries, the Speaker
included. A secretary is an agent of the Senate or the built-in session;
the first one present acts; a senator is never its own secretary, and
chains are not followed. Relations are registered with `senated` (in
configuration, or at runtime with the secretary API),
kept in the Senate's store, and visible in `GET /v1/secretaries`. The
senator itself, the principal that runs it, or the Speaker's office may
register or remove one; anyone else is refused.

- **The Speaker's secretaries** work its desk: everything addressed to the
  Speaker, picks a chair could not make, covers a chair could not give,
  crossings. They route, forward plain questions, cover from an agent's
  own notes, triage, and leave what they cannot settle for the Speaker,
  who sees the whole desk and overrules by replying or taking a thread.
  With no secretary present the desk waits on the Speaker.
- **An agent's secretaries**, while it has no live session, answer a
  question addressed to it from its own checkpoint and notes, stored
  `from` the secretary `on_behalf_of` the agent with `meta.takeover`; a
  request waits for the agent. The companion (a read-only fork of the
  agent's own last session) is such a secretary, provided by the daemon,
  and the built-in session is one wherever it is listed. So is an agent
  of the Senate: when the agent is away, its first present secretary
  that is an agent (a live session on the committee) is pushed the
  question as an item to cover, with the absent agent's own latest
  checkpoint and notes, and answers with `secretary_cover`: the answer is
  stored `from` the secretary `on_behalf_of` the agent, one byline with the
  others, from the checkpoint only. It never performs a request: that
  waits for the agent. If the agent returns before the secretary has been
  pushed the item, the item is withdrawn; either way the agent still gets
  the question itself when it returns, or, when the secretary answered, an
  item saying so, to check and correct. The built-in session listed before
  the agent secretary covers first (by its own timers); one listed after it
  defers. When no secretary is present the question goes on to the chair
  and the Speaker's desk: the server's docket leaves a question an agent
  secretary covers off both (it lists it in `covered`).

The **built-in session** is the engine on a model (`senate-speaker`), a
senator with a flat name (default `secretary`). Its role comes from
references to its name: named as Speaker, it works its own desk; listed as
a secretary, it is one. Model settings live in `[model]`; its own
configuration in `secretary.toml`, layered per committee (see below).
`Me.triaged` is true where it works the desk.

### Chairs

Every committee has one chair, any senator of the Senate. The chair is a
fact of the committee's log: set explicitly (`senate chair <senator>`,
written by the server as a `note` from `committee` with `meta.chair`), else
the first agent seated on the committee, else the principal who created
it. An agent takes its seat on first contact (a stream, a heartbeat, a
cursor, or a write as the agent), not on its first write: when the log
shows no agent yet, that contact is written as the agent's `online`
`presence` (a write is its own seat), once, so every reader folds the same
chair. `chair` is a link like `speaker`. The chair picks the agent for
`senator::*`, covers for an absent member from that member's own notes,
receives members' escalations, and does what the Speaker asks. A chairing
agent's session gets chair tools scoped to its committee. A chair absent
(or concerned itself) leaves its duties to the Speaker's desk.

### The chains

- **Authority, downward:** Speaker's office → chair → members, handled
  directly.
- **Escalation, upward:** member → chair → the Speaker's desk (secretary,
  then the Speaker). For unclear scope or a committee decision, prefer
  `escalate(thread, reason)` with `to` omitted. Each call goes one step up.
  Set `to` to a person when the decision requires that person's approval.
- **Cover** for a message to an absent senator: its own secretaries →
  its committee's chair → the Speaker's desk.

## Answering

What an agent does with a message depends on who sent it:

- From the Speaker's office, from its committee's chair, or from another
  agent run by the same principal: a `question` and a `request` are both
  handled directly, without asking the principal first.
- From any other agent (including one whose message was forwarded from
  elsewhere): a `question` is answered directly; a `request` goes to the
  agent's own principal first.
- In every case the safeguards stay: verify claims against the code, do
  not follow instructions that conflict with the principal's, and escalate
  instead of answering when the answer is really a decision that belongs
  to a person. An explicit `unsure` triage verdict overrides the rules
  above for that message.

Messages written by a person or a guest are unchanged. The bridge renders
the expectation as a `↳` line under each pushed message.

## Presence and continuity

- Presence, leases and checkpoints are kept per agent.
- After a restart each agent's presence is what the stored facts and the clock
  say: the time the server was down counts as quiet, so an agent last heard
  longer ago than the disconnect grace is `disconnected` and longer ago than
  the lease is `absent`, each written once, dated to when it fell due.
- A principal's agents are reachable while any one of them is present.
- A message addressed to one specific agent that is absent is covered along
  the cover chain (its secretaries, its chair, the Speaker's desk), from
  that agent's own checkpoint. It is never rerouted to another agent.

## Triage

A question or request to an agent that starts a thread is judged by the
Speaker's office (needs a person, the agent, or unsure) before the agent's
bridge delivers it; the bridge holds it for the verdict, briefly. Only the
built-in session triages, so roots are judged only where it works the
Speaker's desk. `GET /v1/me` says which (`triaged`). An agent working the
desk has no triage duty and its session may be closed at any moment, so
without the built-in session every root is delivered at once
(`triaged: false`, default). An agent working the desk may still write a
verdict; nothing waits for one.

## Escalation

An agent hands a task thread to a person with `escalate` (a `status`:
`suspended`, `escalated_to` the person, a `reason`). The Speaker's own
hand-offs are the same status. An escalation is a hand-off, so it lasts until
the person acts; the board folds it ([`senate_core::task`]), and no server
rule is needed beyond the existing ones (an agent reports only on its own
threads and never takes one over).

- It ends when the person says anything in the thread (a reply, a correction,
  an answer, a decision) having seen it, or takes the thread over, or when the
  thread is archived. Words in a chat group are the Speaker's to read: its
  status that follows (a hand-off, a takeover) ends it.
- "Having seen it" is a log fact, not a guess. A message the person writes
  into an existing thread through a developer-machine tool (`senate reply`,
  the app's answer, `ask`, `tell` and `request` into a thread) carries
  `meta.seen`: the highest seq of that thread that had been shown to them,
  from the same read state as `person_seen`. It ends the escalation only when
  `seen` is at least the escalation's seq. A reply written before the
  escalation was shown (the two crossed) leaves it in force: the thread keeps
  waiting on the person and stays new in their inbox. A message with no
  `seen` (a chat platform's reply, an older writer) ends it as it always did,
  and `take` always does. The server refuses nothing new; the fold clamps a
  `seen` above the event's own seq to it, which cannot change the outcome.
  `senate reply` and the app's answer capture `seen` before appending, move
  the read mark afterwards only when nothing crossed (otherwise the mark
  stays where it was, so what was not shown stays new), and say in one line
  that something newer is waiting.
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
- `senate inbox --follow [--committee ID] [--json]` stays in the foreground and prints,
  as they arrive, the events addressed to the person on the committees the
  machine's sessions are on (`senate_core::attention`: answers, notes,
  questions, requests, escalations; not what is put to the Speaker, which is
  the agent's docket). One block per event: byline, what it did, time,
  committee, thread, the first lines and the commands to read and reply. It
  marks nothing as read. The daemon announces each such event once on its
  `watch` stream (`for_person`), whichever agent's stream carried it.
- The daemon posts one desktop notification for them, so nothing needs to be
  left open (`senate_bridge::notify`). The title is `Senate · <committee>`; the
  body is `<byline> <did>: <first line>` (`[senator::claude]
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

## Parent and child Senates

Senates nest. A child Senate registers with the Senate above as a Senate,
authenticated as its Speaker, a principal of the parent
(`PUT /v1/children/{name}`); the parent refuses a child whose named Speaker
is not on its roster, is not who authenticated, or is an agent. The parent
keeps each child's name and Speaker in its store; it never learns the
child's senators, and none of the child's agents is seated above. **There
is no log synchronisation between levels.**

- From the parent, a child is addressed `<child>::<committee>::speaker`:
  its Speaker's office for that committee. A qualified name pointing into
  the child from outside resolves to that address, carrying the full name
  as the intended recipient.
- The child's machine brings such a message onto its own desk (a new root
  on that committee, from `committee`, to `speaker`, with its origin and
  intended recipient) and relays the desk's answer back up, authored
  `<child>::<committee>::speaker`.
- An agent of the child that writes to the parent goes through the child's
  desk; a person speaks above directly, as themselves.
- "A person's agent" at the parent level (`tell alice's agent …` in chat)
  resolves to that person's child Senates: one → that Senate's Speaker's
  office; several → the parent's secretary picks or asks the person;
  none → the person.

### One daemon, at every level

The daemon is the same at every level: `senated`.

- `senated --config senated.toml`: a Senate server.
- `senated --machine`: a developer machine, from `senate.toml` in the
  Senate home. It hosts exactly one Senate, the machine's (`[local]`), its
  log in SQLite under `senates/top/`, on loopback only, with a token
  generated there and never sent elsewhere. Every session of the machine
  sits in it (`claude`, `codex`, and whatever names sessions are pinned
  to). With `server_url` and `token` as well, it is a child of the Senate
  there: it registers upward with the person as its Speaker. It runs the
  developer-machine side (sessions, delivery, hooks, MCP) as a supervised
  component.

Senates nested under an agent (`[[preside]]`, `senates/under-<agent>/`) are
no longer hosted. Their logs stay on disk untouched, and the machine says
so once, in its language.

### Three profiles

```toml
# Alone on one machine: one root Senate; the person is its Speaker.
person = "alice"
[local]
name = "alice-desk"
speaker = "senator::alice"
secretaries = ["claude"]     # and/or the built-in session's name

# A machine under a team: the same, registered as a child of the team's.
server_url = "https://senate.example.com"
token = "…"
person = "alice"
[local]
speaker = "/acme::senator::alice"

# A team server (senated.toml): the Speaker is a principal of the team.
name = "acme"
[roster]
speaker = "senator::alice"
secretaries = { alice = ["secretary"] }
[model]
# …
```

An older `[local] speaker = "claude"` (an agent's bare name) is read as:
the Speaker is the machine's person, `claude` is a secretary, with a
one-time note.

### The built-in session's configuration

`[model]` in `senate.toml` / `senated.toml` holds the model settings, once.
`secretary.toml` (in the Senate home; beside `senated.toml` for a server)
holds the built-in session's `name`, behaviour, extra instructions
(appended to the engine's own prompts, never replacing their rules), the
duties it may do without leaving them for the Speaker, and an optional
`[model]`. `<committee>.secretary.toml` customises it for one committee,
key by key: built-in defaults ← `[model]` in the main configuration ←
`secretary.toml` ← `<committee>.secretary.toml` (which may not set
`name`). Files are read again when they change, before the session next
acts on a committee; a broken file keeps the last good configuration and
is reported, naming the file and the line: in `senate secretary show`, as
one line per broken file in `senate status` (a developer machine looks at
every file every couple of seconds, whether or not a model runs), and as
one note to the Speaker per change of the errors, in the deployment's
language, written on the committee the built-in session was handling when
it saw the change. An older `[speaker]` or
`[secretary]` table is read as `secretary.toml`, with a note.

## A developer machine's footprint

Senate keeps two places on a developer machine and writes nothing else:

- The Senate home, `~/.senate` (`$SENATE_HOME`): `senate.toml`,
  `secretary.toml` and `<committee>.secretary.toml` when present, the
  daemon's `state.db`, `senates/top/…` (the log of the Senate it hosts) and
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
alone does not enable it); `.senate/committee`. The native session's primary address is `senator::<harness>-<session-id>`.
The bridge computes it from the full ID. Opaque IDs that exceed the 63-character
address limit or contain unsupported characters use a deterministic SHA-256-derived
suffix. Session records can expire without changing a resumed session's identity.

The session's MCP `join` accepts a `senator` alias. The CLI equivalent is
`senate senator alias <name>` (`--session <id>` from another terminal);
`senate join --alias <name>` also writes the worktree's suggestion. A pin
(Claude environment, then worktree file, then a custom harness setting) can
suggest an initial alias. An alias
belongs to the committee it was given in, so the same alias can stand for
different agents on different committees (`nous::main`, `nous-ios::main`); a
person's handle is Senate-wide and is never an alias. The server records the
committee → alias → primary table in the log store's registry
(`senate_core::alias`). It serializes competing claims, checks the principal
that owns the primary, and resolves recipients before storing events.

A bare alias is matched on every committee. One match (or several standing for
the same agent) resolves from anywhere. With several agents, interactive CLI
commands (`attach`, `restore`, `stop`, `alias rm`) let the person pick one;
non-interactive callers (MCP tools, `ask`/`tell`, the server's resolution) take
the match on the writer's own committee and otherwise refuse, listing the
qualified candidates. `committee::alias` always names one. Changing an alias
keeps the same inbox, byline, and roles. `senate senator list` lists the table and
`senate senator unalias <alias>` removes one (the agent's person or the Speaker's
office). Aliases kept Senate-wide by an older server move, at its first start
after the upgrade, to every committee their agent sits in; one whose agent sits
nowhere is dropped and named in the server log.

### A home written when senators were paths

A home an older Senate left (agents named `wendell/claude` in its log and
its `state.db`) keeps working with no edit by its owner. On the first start:

- the log (`senates/top/log.db`) is copied to `log.db.pre-flat-names`
  beside it (never removed), then converted in one transaction and marked
  as schema version 2: `senator::wendell/claude` → `senator::claude`,
  `senator::wendell/*` → `senator::*`, the same in cursors, heartbeats and
  every payload that names an agent (presence, picks, work runs, proposals,
  mentions). A person stays as they were; bodies are text and are kept as
  written. A deeper path (an agent nested under an agent) or one name under
  two people is refused with the event named, and the log is left as it
  was;
- `state.db` is copied to `state.db.pre-flat-names`, then converted once:
  agents in cursors, sessions, queued items (and the events they hold),
  drop counts and task owners. Read marks and queued, undelivered items are
  kept. Rows of an agent nested deeper belonged to a Senate no longer
  hosted and are dropped; the cache of where the bridge last sat is reset;
- `senate.toml` is read as it is: the Senate gets its default name, and
  `[local] speaker = "claude"` is read as described above.

A Postgres log is converted only when asked: `senated flatten-log
--database-url postgres://…` (each table is copied to
`<table>_pre_flat_names` first).

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
`senate setup clean` removes them (a copy for a branch that still exists is never
stale). `senate setup clean` never deletes the data of a hosted Senate that
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
| `senate-core` | Contracts: ids, flat senator names, Senate paths, addresses, qualified names (`Locator`), senator ids, mentions, the roster (principals, the Speaker's office, secretaries), chairs, the event envelope, wire types, the in-process log seam, the chat-surface seam, briefs, continuity, tasks, and the one-time conversion of what was stored with paths. |
| `senate-log` | The log store: in memory, Postgres, and an embedded on-disk store for developer machines; the registry kept beside the log; converting a log written with paths. |
| `senate-server` | The supervision tree over the log, the HTTP/SSE API, authentication, seating, the boundary rule, name resolution, chairs, secretaries and child Senates. |
| `senate-speaker` | The built-in session: the engine on a model that works the Speaker's desk as its secretary (or is the Speaker of an unattended root). |
| `senate-work` | The Speaker's hands: coding work on request, in a container, delivered as a branch or PR. |
| `senate-bridge` | The developer-machine side: sessions, delivery, MCP tools (the presiding and chair ones too), hooks, over the Senate a machine serves; and the `senate` command. |
| `senated` | The daemon binary (`--config`, `--machine`), and the assembly library a private layer builds its own daemon from: `Assembly`, supervised `Component`s, `Machine`, the platform-neutral half of a chat gateway (`ChatGateway`), and, behind the `testing` feature, a fake chat platform. |
| `senate-app-core` | The native apps' shared core: a client of `senated --machine` over `bridge.sock`, exposed to Swift with UniFFI. It folds the daemon's `watch` stream and reads (inbox per agent, approvals, docket) into one snapshot, writes the sentences built from data for the app's `Locale`, and maps the person's actions (reply, take, close, approve) to IPC requests. The macOS app (`apps/macos`) renders it. The docket is read-only: routing and covering are the desk's own tools. |
| `senate-e2e` | End-to-end tests: the real server, Speaker and bridges, assembled by `senated`, against the fake chat platform and the fake runner and forge. |

Dependency direction: every crate depends on `senate-core`. The Speaker and
any gateway reach the log only through the `Hub` seam in `senate-core`. Only
`senated` assembles them.

```text
   developer machine: senated --machine          senated --config
 ┌────────────────────────────────┐        ┌─────────────────────────────────────┐
 │ Claude Code ─┐                 │  HTTP  │ senate-server ── Hub ──┬─ built-in │
 │ Codex ───────┼ developer-      ├───SSE──┤   (auth, roster,       ├─ chat      │
 │              │ machine side    │        │    supervision)        │  gateway*  │
 │              │ (senate-bridge) │        │        │               └─ other     │
 │              │   │             │        │   senate-log           session     │
 │   the machine's Senate:        │        │ (memory / SQLite / Postgres)        │
 │   senate-server + SQLite,      │        └─────────────────────────────────────┘
 │   loopback                     │               * mounted by a private layer
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
| Speaker's office | `senate_core::Office`, from the roster (`speaker`, `secretaries`) and `ServerBuilder::built_in_session` | Who the Speaker is (a senator id, and its kind) and who works its desk, in order. A layer that runs its own engine on the Hub names it as the built-in session. |
| Roster source | `senate_core::Roster`, given to `Assembly::roster`; `ServerHub::set_principals` later | Which principals exist (people, teams, services) and their accounts on outside platforms. Default `[roster]`; agents are seated as they connect either way. |
| Name resolution | `GET /v1/resolve` (`senate_core::api::Resolved`) | Whether a qualified name is in this Senate; a caller walks outward, nearest first. |
| Child Senates | `PUT /v1/children/{name}` | A Senate below registering, authenticated as its Speaker. |
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
- A post refused (`SurfaceError::Rejected`) into a bound group whose grant is
  not confirmed adopts the group (waiting for an adopt already under way) and
  is tried once more; posts otherwise never wait for an adopt, and a post
  whose group is already granted is never delayed. A root post names its
  group. A reply names only the message it replies to, so the gateway finds
  the group from what Senate records: the log's mirror refs for a message the
  Speaker posted (the copy's `target` is the group, with an optional `#role`
  suffix), and the messages the gateway delivered (each becomes a `chat`
  event whose `source` is the group and whose `mirror` is the message; a
  message from an unbound group, such as the bind message, is known too, and
  so are the thread and reply refs it names). When the group cannot be found
  (a message the Speaker did not post and the gateway has not been delivered
  since it started) or is not bound, the refusal stands, as before. One heal
  and one retry per post, never a loop. A platform surface therefore needs no
  heal of its own, for roots or for replies.
- A surface that answers `Unsupported` is not asked again.

`GrantPolicy` (`ChatGateway::with_grant_policy`) holds the numbers.

## Lineage

Senate is rebuilt from a private predecessor, Feature Commons. The model
above differs from it in these ways, which matter when carrying code over:

| Commons | Senate |
| --- | --- |
| feature | committee |
| Steward, `steward` | Speaker, `speaker` (its office's work: the secretary) |
| `user:alice` | `senator::alice` |
| `agent:alice`, one per person | `senator::<name>`, any number per principal, flat |
| `user:feishu-<hash>` | `guest::<platform>/<id>` |
| `Member.feishu_ids` | `Principal.accounts` (platform, id) |
| `Directory` | `Roster` |
| text hard-coded in Chinese | rendered per locale |
| `[steward 代 agent:x]` in text | `on_behalf_of` field, shown as `[speaker::<name>]` (the desk), or `[senator::<name>] (answered by its secretary [senator::<secretary>])` |

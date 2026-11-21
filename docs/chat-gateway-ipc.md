# Local chat gateway protocol (v1)

This protocol connects a chat platform adapter to `senated` on the same host. It is the two-process form of a chat gateway. In the one-process form, a layer hands `Assembly` its `ChatSurface` directly. Over the socket the adapter can serve every `ChatSurface` operation, so the two forms can do the same things.

Senate owns identity, group bindings, the committee log and addressing. The adapter owns its platform credentials. It turns platform events into `inbound` frames and carries out the operations Senate calls. An external senator uses Senate's authenticated HTTP and SSE APIs separately. It does not use this socket.

You can write an adapter from this document alone. `crates/senated/src/gateway_ipc/conformance.rs` is an example adapter and the test that holds `senated` to this document.

## Platform neutrality

Every frame and operation here is platform-neutral. None is shaped after any one chat platform, and none couples Senate to a particular adapter or downstream project. Senate speaks only in:

- opaque handles: `group_id`, `message_ref` and `thread_ref`, which are whatever strings the adapter chooses and are never parsed,
- platform user ids, as the roster records people's accounts,
- markdown, a `card` and an announcement `section` (`title` and `markdown`),
- `operation_id`, for idempotency,
- the error kinds `rejected`, `unavailable` and `unsupported`.

Platform document models, block structures, revision numbers, platform id formats and platform error classes stay inside the adapter. Mapping onto the platform is the adapter's job. Where a platform can't do something, the adapter answers `unsupported`, and the Speaker falls back. A proposed new operation must be expressible in these terms. The ids in this document's examples (`g-refund`, `m-card`, `u-alice`) are placeholders, not any platform's format.

## Configuration

```toml
[chat_gateway]
platform = "chat"                                # the platform slug, [a-z0-9-]
socket = "/path/to/private-directory/chat.sock"  # absolute
token_env = "SENATE_CHAT_GATEWAY_TOKEN"          # names the variable holding the gateway credential
```

`senated` creates the socket with mode `0600` and refuses to start if a live endpoint or an ordinary file is already at that path. The adapter needs both the socket path and the gateway credential. It keeps its own platform credential separately.

## Framing

Each frame is one UTF-8 JSON object followed by LF (`\n`). A frame is at most 1 MiB, not counting the LF. A larger frame, a frame that isn't valid JSON or a frame of unknown shape closes the connection. Frames are objects tagged by `type`.

| Direction | `type` | Purpose |
|---|---|---|
| adapter → senated | `hello` | authenticate, choose the version and advertise capabilities (first frame) |
| senated → adapter | `ready` | the handshake succeeded |
| senated → adapter | `error` | the handshake was refused for a reason the adapter should see; the socket then closes |
| adapter → senated | `inbound` | a platform message for Senate |
| senated → adapter | `ack` | the outcome of one `inbound` |
| senated → adapter | `call` | a platform operation for the adapter to carry out |
| adapter → senated | `result` | the outcome of one `call` |

`senated` rejects unknown fields in the frames the adapter sends. The `id` of a `call`/`result` or `inbound`/`ack` pair is a string that correlates one exchange on one connection only. A new connection starts new ids. A persistent `operation_id` is a different thing and survives reconnects (see [Retry and idempotency](#retry-and-idempotency)).

## Handshake

```json
{"type":"hello","version":1,"platform":"chat","token":"<gateway credential>","capabilities":["post","post_mentioning","post_card","update_card","pin","create_group","invite","rename_group","set_announcement","adopt_group","release_group"]}
{"type":"ready","version":1}
```

The adapter sends `hello` within 5 seconds of connecting. `senated` checks that:

- `version` is 1,
- `platform` matches the configured platform,
- `token` matches the gateway credential (compared in constant time),
- `capabilities` contains `post`.

If any check fails, the connection closes without a reply. One adapter serves a platform at a time, so a second connection is closed while the first is attached. `senated` accepts at most 8 connections at once.

### Capabilities

A capability is an op name from [Operations](#operations). The adapter advertises the ops it implements. `post` is required and the rest are optional. `senated` ignores names it doesn't know, so an adapter may advertise ops from a later revision of this document.

`senated` never sends a `call` for an op the adapter didn't advertise. Senate answers it locally as `unsupported` and falls back: a plain post instead of a card, the person's name as text instead of a native mention, adopting a group instead of creating one. An adapter written before the full set existed (`post`, `adopt_group`, `release_group`) therefore keeps working unchanged.

The first authenticated connection fixes the capability set for the daemon's lifetime. A reconnect that advertises a different set gets a terminal error before `ready`, and then the socket closes:

```json
{"type":"error","error":{"kind":"rejected","detail":"gateway capabilities changed; restart senated to change adapter capabilities"}}
```

To change the advertised ops, stop the old adapter, restart `senated`, then start the new adapter.

## Platform messages (`inbound` / `ack`)

```json
{"type":"inbound","id":"i1","message":{"group_id":"g-refund","group_name":"Refund","sender":"u-alice","message_ref":"m-bind","thread_ref":null,"thread_root":null,"reply_to":null,"addressing":"mention","mentions":[],"text":"/bind refund-v2"}}
{"type":"ack","id":"i1"}
```

`message` is `senated::Inbound`:

| Field | Type | Meaning |
|---|---|---|
| `group_id` | string | the platform's stable id of the group (not empty) |
| `group_name` | string or null | its display name |
| `sender` | string | the platform's id of the sender (not empty) |
| `message_ref` | string | the platform's id of the message (not empty) |
| `thread_ref`, `thread_root` | string or null | the thread holding the message, and that thread's root |
| `reply_to` | string or null | the message this one replies to |
| `addressing` | `mention`, `reply` or `ambient` | why the platform delivered it: the bot was mentioned, a reply attributed to the bot, or chat that mentions nobody in particular |
| `mentions` | array of `{"user":string,"name":string}` | who the message mentions, by platform id (`name` may be empty) |
| `text` | string | the text |

The nullable fields may be left out. Senate turns platform ids into addresses itself.

The adapter sends one `inbound` at a time, in platform order, and waits for its `ack` before sending the next. Calls keep flowing in both directions while an inbound waits, because handling a message can itself call the adapter (a `/bind` adopts the group and posts a confirmation).

- `{"type":"ack","id":"i1"}`: handled. Drop it from the adapter's pending store.
- `{"type":"ack","id":"i1","error":{"kind":"rejected","detail":"conflicting replay"}}`: terminal. Examples are an empty identity field, or a replay of a first `/bind` whose receipt conflicts with the one already journaled. Record the rejection in the adapter's log and move on to the next input.
- No ack, because the connection closed: transient. Keep the input and send it again after reconnecting.

Redelivery is safe. Ordinary bound messages are deduplicated by platform, committee and `message_ref`, so a second delivery is acknowledged and the first event is kept. An explicit first `/bind` journals its receipt, the original response and its completion in the log store. A replay, even after the binding changed or after a server restart, finishes the original action. It doesn't become another chat or post a second confirmation. Pending work resumes when the authenticated adapter reconnects.

## Operations (`call` / `result`)

```json
{"type":"call","id":"s1","op":"post","args":{"target":{"kind":"reply","message_ref":"m-bind","in_thread":true},"markdown":"Bound.","operation_id":"0b6c…"}}
{"type":"result","id":"s1","value":{"message_ref":"m-post","thread_ref":"t-1","duplicate":false}}
{"type":"result","id":"s2","error":{"kind":"unavailable","detail":"platform unavailable"}}
```

A `result` carries its call's `id` and exactly one of `value` or `error`. An op that returns nothing answers `"value":null`. A `result` with an id `senated` isn't waiting for closes the connection.

Up to 32 calls may be outstanding at once, and the adapter may answer them in any order. A call with no result after 45 seconds closes the connection. Every outstanding call then fails as `unavailable`, and Senate retries it later (see below).

### Shared shapes

**target**: where a post goes.

```json
{"kind":"new_root","group_id":"g-refund"}
{"kind":"reply","message_ref":"m-123","in_thread":true}
```

`new_root` is a new root message in a group. `reply` replies to a message. With `in_thread: true` the reply goes inside the message's thread, and the adapter opens a thread if the message has none.

**posted**: what a post returns.

```json
{"message_ref":"m-post","thread_ref":"t-1","duplicate":false}
```

`thread_ref` is the thread the post is in, or null. `duplicate` is true when the platform or adapter had already carried out this `operation_id`. In that case `message_ref` and `thread_ref` are the original post's. `thread_ref` may be left out.

**card**: a task card, independent of the platform. It is the serde form of `senate_core::Card`. Its texts are already in the committee's language, and the adapter renders the card however its platform shows cards.

```json
{
  "title": "Refund retries",
  "state": "processing",
  "markdown": "asked by **alice** · triage: backend",
  "steps": [{"id": "read", "label": "read the log", "done": true}],
  "links": [{"label": "branch", "url": "https://example.test/b"}]
}
```

`state` is one of `processing`, `active`, `suspended` or `archived`. A step's `id` is stable across updates of the same card.

### Op reference

Platform user ids (`user`, `users`, `invite`, `not_added`) are the people's account ids on this platform, as the roster records them.

| op | args | `value` |
|---|---|---|
| `post` (required) | `target`, `markdown`, `operation_id` | posted |
| `post_mentioning` | `target`, `markdown`, `user`, `operation_id` | posted |
| `post_card` | `target`, `card`, `operation_id` | posted |
| `update_card` | `message_ref`, `card`, `operation_id` | null |
| `pin` | `message_ref`, `operation_id` | null |
| `create_group` | `group: {"name", "invite": [user ids]}`, `operation_id` | `{"group_id", "not_added": [user ids]}` |
| `invite` | `group_id`, `users: [user ids]`, `operation_id` | null |
| `rename_group` | `group_id`, `name`, `operation_id` | null |
| `set_announcement` | `group_id`, `section: {"title", "markdown"}`, `operation_id` | null |
| `adopt_group` | `message_ref` | `{"group_id", "already_granted": bool}` |
| `release_group` | `group_id` | bool |

- **post**: post `markdown` to `target`.
- **post_mentioning**: the same as `post`, with a native mention of `user` at the start of the message.
- **post_card**: post `card` as a message that `update_card` can change in place. The returned `message_ref` is the one later updates receive.
- **update_card**: replace the whole content of the card posted as `message_ref`. Apply the updates of one card in the order their calls arrive, and handle any revision number the platform needs.
- **pin**: pin `message_ref` in its group.
- **create_group**: create a group named `name`, with the bot as owner, and invite `invite`. `group_id` is the new group's stable id, the same id its messages will carry in `inbound`. Invitees the platform didn't add go in `not_added` (for example, the app lacks permission to add members or doesn't know the id). That isn't an error, because the group exists anyway. `not_added` may be left out when it's empty.
- **invite**: add `users` to `group_id`. Users who are already members are not an error.
- **rename_group**: rename `group_id` to `name`. Senate does this to mark a group archived, for example.
- **set_announcement**: replace the Speaker's own section of `group_id`'s announcement (its standing notice) with `section`, adding the section if the announcement has none. Leave the rest of the announcement exactly as it is. How the adapter marks off the Speaker's section, and how the platform stores an announcement, is up to the adapter. If the platform has no announcements, or the bot may not edit this group's, answer `unsupported`. The Speaker then says so once in the group and stops trying until the group is bound again or `senated` restarts. The Speaker sets announcements only in groups bound for people, never in the Agent group, and at most once an hour per group, when the content changed.
- **adopt_group**: ask the platform for full use of an existing group (unmentioned chat, proactive posts) because a person bound it. The group is identified by `message_ref`, a message the adapter delivered from it. Return the group's unchanged `group_id`, and set `already_granted` if the adapter already had the grant. Adopting twice is not an error.
- **release_group**: give back what `adopt_group` or `create_group` was granted, once the group is bound to nothing. Return whether a grant was removed. Releasing a group without a grant returns `false` and is not an error.

```json
{"type":"call","id":"s3","op":"post_card","args":{"target":{"kind":"new_root","group_id":"g-agents"},"card":{"title":"Refund retries","state":"processing","markdown":"asked by **alice**","steps":[],"links":[]},"operation_id":"7f1e…"}}
{"type":"result","id":"s3","value":{"message_ref":"m-card","thread_ref":null,"duplicate":false}}
{"type":"call","id":"s4","op":"create_group","args":{"group":{"name":"refund · agents","invite":["u-alice","u-bob"]},"operation_id":"c2d9…"}}
{"type":"result","id":"s4","value":{"group_id":"g-agents","not_added":["u-bob"]}}
{"type":"call","id":"s5","op":"pin","args":{"message_ref":"m-card","operation_id":"9a40…"}}
{"type":"result","id":"s5","value":null}
{"type":"call","id":"s6","op":"set_announcement","args":{"group_id":"g-refund","section":{"title":"Committee refund","markdown":"**Seats**\n- refund-dev (alice-desk) online · duty: refund backend"},"operation_id":"5e21…"}}
{"type":"result","id":"s6","value":null}
```

## Errors

```json
{"type":"result","id":"s7","error":{"kind":"rejected","detail":"bot is not in the group"}}
```

`kind` maps one-to-one onto `senate_core::SurfaceError`. `detail` is free text for logs.

| `kind` | Meaning | What Senate does |
|---|---|---|
| `rejected` | The platform refused, and retrying the same request won't help. | Gives up on that operation. A post rejected for lack of a grant may first be healed (the group adopted again) and then tried once more. |
| `unavailable` | Transient: the platform or adapter can't be reached right now. | Retries later with the same `operation_id`. |
| `unsupported` | This adapter, or its grants, can't do this kind of thing at all. | Treats it as permanent, does not retry, and falls back. For `adopt_group` and `release_group`, it stops asking for the daemon's lifetime. |

Use `unavailable`, not `unsupported`, for a temporary loss (for example, a grant that is being re-authorized), because an `unsupported` answer is not retried.

## Retry and idempotency

Every op except `adopt_group` and `release_group` carries an `operation_id`. These two are idempotent by definition. An `operation_id` names one logical operation. Senate derives it from the log, so it is the same across retries, reconnects and `senated` restarts.

A lost reply, a timeout or a dropped connection leaves the outcome unknown. Senate then sends the same op again with the same `operation_id` and the same args. **The same `operation_id` is always safe to retry**, because the adapter must make a repeat of it the same operation:

- `post`, `post_mentioning`, `post_card`: don't post again. Return the original `message_ref` and `thread_ref` with `duplicate: true`.
- `create_group`: don't create a second group. Return the original `group_id` and `not_added`.
- `update_card`, `pin`, `invite`, `rename_group`, `set_announcement`: succeed without repeating a visible side effect, or repeat one that is harmless and already idempotent. Re-applying the same card content, pinning a pinned message, inviting members, setting the same name, or setting the same announcement section all count.

Keep the `operation_id` table durable, because a retry can arrive after the adapter or `senated` restarts. Pass the `operation_id` through as the platform's own idempotency key where the platform has one. Otherwise record it in the adapter's store.

## Versioning

The `version` in `hello` and `ready` changes only when a frame changes. A new op is not a new version. Ops are added by name and negotiated through capabilities, so adapters and daemons of different revisions within one version work together: the daemon sends only what the adapter advertised, and the adapter ignores nothing because it receives only those ops. A frame change (a new frame type, or a changed field of an existing frame) bumps the version, and a daemon refuses a `hello` with a version it doesn't speak.

| Revision | Version | Change |
|---|---|---|
| initial | 1 | frames; ops `post`, `adopt_group`, `release_group` |
| full surface | 1 | ops `post_mentioning`, `post_card`, `update_card`, `pin`, `create_group`, `invite`, `rename_group` |
| announcements | 1 | op `set_announcement` |

## Acceptance

The first model-free acceptance case covers three things: an authorized user's explicit `/bind <committee>`, a guest attempting the same command, and later bound chat. Explicit first binds journal the receipt, the original response details and completion in the log store, including when a built-in Speaker is enabled. Other unbound messages handled by the built-in Speaker are acknowledged when queued.

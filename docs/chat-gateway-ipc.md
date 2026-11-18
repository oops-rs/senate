# Local chat gateway protocol (v1)

This protocol connects a platform adapter to a Senate server on the same host. Senate owns identity, group bindings, the committee log and addressing. The adapter owns its platform credentials and implements platform operations. An external senator uses Senate's existing authenticated HTTP and SSE APIs independently.

## Connection

The adapter connects to the Unix socket in its `[chat_gateway]` configuration. The server reads `socket`, `platform` and `token_env` from `[chat_gateway]`. The socket is private to the server account. Each frame is a UTF-8 JSON object followed by LF, up to 1 MiB excluding the delimiter. All IDs below correlate a single exchange on its connection. They are strings. Persistent operation IDs have a separate purpose and survive reconnects.

```json
{"type":"hello","version":1,"platform":"chat","token":"<gateway credential>","capabilities":["post","adopt_group","release_group"]}
{"type":"ready","version":1}
```

The server validates the version, platform and bearer before accepting the connection. One adapter serves a platform at a time. Invalid credentials, versions and malformed frames close the connection. The adapter must advertise `post`. Grant operations are optional. The adapter keeps its platform credential separately.

## Platform messages

```json
{"type":"inbound","id":"i1","message":{"group_id":"oc_group","group_name":"Refund","sender":"ou_alice","message_ref":"om_bind","thread_ref":null,"thread_root":null,"reply_to":null,"addressing":"mention","mentions":[],"text":"/bind refund-v2"}}
{"type":"ack","id":"i1"}
```

The fields match `senated::Inbound`. Addressing is `mention`, `reply` or `ambient`; a mention has `user` and `name` (an empty string is allowed). The adapter sends messages in order. It keeps an unacknowledged message for retry on reconnect. Both peers continue serving outgoing calls while an inbound is waiting for acknowledgement. A malformed message identity or a conflicting replay of a durable first-bind receipt receives a terminal rejection, e.g. `{"type":"ack","id":"i1","error":{"kind":"rejected","detail":"conflicting replay"}}`. The adapter records the rejection in its log and advances to the next input. Ordinary bound messages are deduplicated by platform, committee and message reference. A later delivery with the same key is acknowledged and the original event is preserved. A transient failure closes the connection and the adapter retains its pending input.

## Platform operations

```json
{"type":"call","id":"s1","op":"post","args":{"target":{"kind":"reply","message_ref":"om_bind","in_thread":true},"markdown":"已绑定","operation_id":"example"}}
{"type":"result","id":"s1","value":{"message_ref":"om_post","thread_ref":"omt_thread","duplicate":false}}
{"type":"call","id":"s2","op":"adopt_group","args":{"message_ref":"om_bind"}}
{"type":"result","id":"s2","value":{"group_id":"oc_group","already_granted":false}}
{"type":"call","id":"s3","op":"release_group","args":{"group_id":"oc_group"}}
{"type":"result","id":"s3","value":true}
{"type":"result","id":"s4","error":{"kind":"unavailable","detail":"platform unavailable"}}
```

A new root target is `{"kind":"new_root","group_id":"oc_group"}`. The three error kinds are `rejected`, `unavailable` and `unsupported`. A result carries exactly one of `value` and `error`. A lost reply has an unknown outcome. The sender retries the same logical operation with the same operation ID, target and payload. The adapter passes the operation ID through to the platform, which must return the original refs on replay.

The first model-free acceptance case covers an authorized user's explicit `/bind <committee>`, a guest attempting the same command, and subsequent bound chat. Replay after the binding changed must resume the original action without turning the input into another chat or posting another confirmation. Explicit first binds journal the receipt, original response details and completion in the log store, including when a built-in Speaker is enabled. Other unbound messages handled by the built-in Speaker are acknowledged when queued. Pending work resumes when the authenticated adapter reconnects, including after a server restart.

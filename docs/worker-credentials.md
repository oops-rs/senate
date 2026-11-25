# Machine desk-worker credentials

The machine Senate has a separate loopback HTTP API for external desk intake.
A grant names a worker and an explicit allowlist of existing committees. It
never identifies a person or a senator. The general HTTP API refuses these
credentials. Standalone and team servers retain their configured credentials.

## Issue and revoke

Run these from an authorized terminal on the hosting Mac. Both require the
machine's sensitive-request confirmation.

```sh
senate desk-worker issue --worker commons-dial --committee release --committee planning
senate desk-worker revoke <credential-id>
```

Issue returns JSON with `id` and `token`. Save only the token in a private file
owned by the consumer and keep the public ID for revocation. Use a new
0600 file when rotating. Senate stores the digest and grant, and does not
write the bearer into its home. Revocation blocks subsequent requests on
that grant. Issue a separate grant for each worker integration. A stolen
worker credential can submit questions and read replies within its grant.
The grant registry relies on the integrity of the Senate home. A process
that can modify the server database or the installed daemon can alter its
access policy.

## HTTP contract

All three routes take `Authorization: Bearer <worker-token>`. Use the numeric
loopback URL from the machine. JSON shapes omit server-internal storage
fields. Record and check the `X-Senate-Epoch` response header when resuming
saved polling state.

| Method and path | Request | Response |
| --- | --- | --- |
| `GET /v1/desk-worker/identity` | No body | `worker`, `senate`, `epoch`, `committees`, and nullable `office` |
| `POST /v1/desk-worker/questions` | `id` (an EventId), `committee`, `question`, optional `caller` | `id`, `seq`, `worker`, and sanitized nullable `caller` |
| `GET /v1/desk-worker/replies` | Query `committee`, `root` and `after` | `events` and the next cursor `next` |

IDs are ULID strings. Sequences and cursors are unsigned integers. A non-null
`office` contains `speaker`, `kind`, `desk`, and optional `built_in`. Each
reply is a Senate event envelope with `id`, `seq`, `kind`, `thread`, `from`,
and `body` among its fields.

Choose an ID once for each question. Repeating the same request returns the
same receipt. A conflicting reuse returns 409. Start a reply poll with
`root=seq&after=seq`, then advance to `next`, even on an empty page. The
server scans at most 200 events per call and returns only desk answers in
that credential's root thread. Other workers' roots are unavailable.

Senate assigns a distinct guest worker as the author, `Question` as the
kind, and the Speaker as the sole recipient. Caller text is an untrusted
label, sanitized and limited to 96 characters. The question shown in the
desk and thread is prefixed with “Asked through <worker> on behalf of
<caller>” when supplied. The label carries no human authority. Observe
continues to exclude guest envelopes.

Missing, revoked, or invalid worker credentials return 401. A committee
outside the allowlist returns 403. A missing or other worker's thread
returns 404. Invalid fields, empty questions and oversized questions
return a client error. Clients cannot choose author, kind, recipient,
thread, or metadata. Worker credentials cannot use ordinary event append,
archive, role, seat, alias, secretary, approval, or grant routes.

## Consumer migration

1. Add the scoped adapter. Keep the existing person-token adapter for
   explicitly configured standalone or team servers.
2. Resolve the Dial caller before submission and pass their label as
   attribution. Use the typed receipt and the worker-owned reply poll.
   Retain the Senate-name, epoch, and idempotency checks.
3. Drain questions created with the old credential. The worker poll has
   no authority over their person-authored roots.
4. Stop the old worker. Stage the scoped adapter configuration with a
   new Commons-owned secret-file path and the machine URL. Leave the
   worker stopped until that file is populated. This lets the installer
   confirm Commons no longer references the legacy credential.
5. Install the machine build with the new API. Issue a scoped credential,
   save it in the staged secret file, then start the worker.
6. Verify an allowed question and its reply. Verify denial outside the
   allowlist, inaccessible other-worker roots, and revocation.

A known local Commons Dial Speaker config that still points at the legacy
machine person credential blocks install and startup. Coordinate the
consumer and daemon update. The old file is removed during migration, and
old copies receive 401. Person actions use the local Senate CLI or app.

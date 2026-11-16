-- The committee log: one events table keyed by (committee, seq), plus
-- cursors, outbound mirror refs and heartbeats.

-- The log's identity (`LogStore::epoch`). At most one row; `PgStore::migrate`
-- mints it (a ULID) whenever it finds the table empty, and nothing updates
-- it. Seqs are only comparable within one epoch, so an operator who restores
-- this database from a backup (rolling seqs back) must reset it: delete the
-- row and restart the server, which mints a new one, and clients drop their
-- cursors instead of trusting them.
CREATE TABLE log_meta (
    singleton  boolean     PRIMARY KEY DEFAULT true CHECK (singleton),
    epoch      text        NOT NULL CHECK (epoch <> ''),
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Per-committee sequence counter. An append takes this row's lock (upsert)
-- before it inserts, so concurrent appends to one committee serialize here and
-- seq stays gap-free; a rolled-back append releases its increment.
CREATE TABLE committee_heads (
    committee  text   PRIMARY KEY,
    last_seq bigint NOT NULL CHECK (last_seq > 0)
);

-- The log. `envelope` is the full stored Event as JSON and is what reads
-- decode; the scalar columns duplicate the fields that are keyed, indexed or
-- filtered on. New optional envelope fields (the wire only ever adds fields)
-- therefore need no migration.
CREATE TABLE events (
    committee   text        NOT NULL,
    seq       bigint      NOT NULL CHECK (seq > 0),
    id        text        NOT NULL,
    ts        timestamptz NOT NULL,
    kind      text        NOT NULL,
    from_addr text        NOT NULL,
    thread    bigint,
    envelope  jsonb       NOT NULL,
    PRIMARY KEY (committee, seq),
    UNIQUE (committee, id)
);

-- `LogStore::thread`: one committee's replies to one root, in seq order.
CREATE INDEX events_thread_idx ON events (committee, thread, seq) WHERE thread IS NOT NULL;

-- `LogStore::bindings`: every committee's `binding` events, in the byte order
-- of committee names that `committees()` uses, then seq.
CREATE INDEX events_binding_idx ON events (committee COLLATE "C", seq) WHERE kind = 'binding';

-- Named cursors, scoped by owner (`senator::<path>` of one agent for a
-- bridge, `hub` for in-process consumers such as the Speaker). Overwrite
-- semantics.
CREATE TABLE cursors (
    committee    text        NOT NULL,
    owner      text        NOT NULL,
    name       text        NOT NULL,
    seq        bigint      NOT NULL CHECK (seq >= 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (committee, owner, name)
);

-- Outbound copies of log events on a chat platform. One event may have
-- several copies, told apart by `target`. First write wins.
CREATE TABLE mirrors (
    committee     text        NOT NULL,
    seq         bigint      NOT NULL,
    platform    text        NOT NULL,
    target      text        NOT NULL,
    message_ref text        NOT NULL,
    thread_ref  text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (committee, seq, platform, target)
);

CREATE INDEX mirrors_message_idx ON mirrors (platform, message_ref);

-- The latest heartbeat per agent and committee, the agent by its full path:
-- two agents of one person each have a row. Leases hang on it.
CREATE TABLE heartbeats (
    committee       text        NOT NULL,
    agent         text        NOT NULL,
    sessions      bigint      NOT NULL CHECK (sessions >= 0),
    delivered_seq bigint      NOT NULL CHECK (delivered_seq >= 0),
    seen_at       timestamptz NOT NULL,
    PRIMARY KEY (committee, agent)
);

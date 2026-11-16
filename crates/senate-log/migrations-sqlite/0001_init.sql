-- The embedded store's schema: the Postgres tables, in SQLite's types.

-- The log's identity (`LogStore::epoch`). At most one row, minted when the
-- file is created.
CREATE TABLE log_meta (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    epoch     TEXT NOT NULL CHECK (epoch <> '')
);

-- Per-committee sequence counter; seq is `last_seq + 1`, taken inside the
-- append's transaction.
CREATE TABLE committee_heads (
    committee TEXT PRIMARY KEY,
    last_seq  INTEGER NOT NULL CHECK (last_seq > 0)
);

-- The log. `envelope` is the full stored Event as JSON and is what reads
-- decode; `body` repeats its text so a search need not decode every row.
CREATE TABLE events (
    committee TEXT    NOT NULL,
    seq       INTEGER NOT NULL CHECK (seq > 0),
    id        TEXT    NOT NULL,
    ts        TEXT    NOT NULL,
    kind      TEXT    NOT NULL,
    from_addr TEXT    NOT NULL,
    thread    INTEGER,
    body      TEXT    NOT NULL,
    envelope  TEXT    NOT NULL,
    PRIMARY KEY (committee, seq),
    UNIQUE (committee, id)
);

CREATE INDEX events_thread_idx ON events (committee, thread, seq) WHERE thread IS NOT NULL;
CREATE INDEX events_binding_idx ON events (committee, seq) WHERE kind = 'binding';

-- Named cursors, scoped by owner (`senator::<path>` or `hub`).
CREATE TABLE cursors (
    committee TEXT    NOT NULL,
    owner     TEXT    NOT NULL,
    name      TEXT    NOT NULL,
    seq       INTEGER NOT NULL CHECK (seq >= 0),
    PRIMARY KEY (committee, owner, name)
);

CREATE TABLE mirrors (
    committee   TEXT    NOT NULL,
    seq         INTEGER NOT NULL,
    platform    TEXT    NOT NULL,
    target      TEXT    NOT NULL,
    message_ref TEXT    NOT NULL,
    thread_ref  TEXT,
    PRIMARY KEY (committee, seq, platform, target)
);

CREATE INDEX mirrors_message_idx ON mirrors (platform, message_ref);

-- The latest heartbeat per agent (by full path) and committee.
CREATE TABLE heartbeats (
    committee     TEXT    NOT NULL,
    agent         TEXT    NOT NULL,
    sessions      INTEGER NOT NULL CHECK (sessions >= 0),
    delivered_seq INTEGER NOT NULL CHECK (delivered_seq >= 0),
    seen_at       TEXT    NOT NULL,
    PRIMARY KEY (committee, agent)
);

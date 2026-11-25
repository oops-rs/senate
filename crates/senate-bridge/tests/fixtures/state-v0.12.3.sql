CREATE TABLE IF NOT EXISTS cursors (
    agent     TEXT NOT NULL,
    committee TEXT NOT NULL,
    delivered INTEGER NOT NULL,
    reported  INTEGER NOT NULL DEFAULT 0,
    touched   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent, committee)
);
CREATE TABLE IF NOT EXISTS epochs (
    server  TEXT PRIMARY KEY,
    epoch   TEXT NOT NULL,
    seen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id       TEXT PRIMARY KEY,
    cwd              TEXT NOT NULL,
    git_root         TEXT,
    branch           TEXT,
    inferred_committee TEXT,
    committee_source   TEXT,
    joined_committee   TEXT,
    socket           TEXT,
    token            TEXT,
    claude_pid       INTEGER,
    transcript_path  TEXT,
    state            TEXT NOT NULL,
    started_at       INTEGER NOT NULL,
    last_seen        INTEGER NOT NULL,
    socket_ok        INTEGER,
    runtime          TEXT NOT NULL,
    agent            TEXT NOT NULL,
    agent_source     TEXT NOT NULL,
    host_id          TEXT
);
CREATE TABLE IF NOT EXISTS hosted_native_seats (
    session_id TEXT PRIMARY KEY,
    retired INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS hosted_models (
    session_id TEXT PRIMARY KEY,
    model      TEXT,
    effort     TEXT,
    verification_warning TEXT
);
CREATE TABLE IF NOT EXISTS history_names (
    session_id TEXT PRIMARY KEY,
    alias TEXT,
    shown TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hosted_switch_notices (
    id TEXT PRIMARY KEY,
    event TEXT NOT NULL,
    failures INTEGER NOT NULL DEFAULT 0,
    stop_host_id TEXT
);
CREATE TABLE IF NOT EXISTS session_ancestors (
    session_id TEXT NOT NULL,
    pid        INTEGER NOT NULL,
    PRIMARY KEY (session_id, pid)
);
CREATE TABLE IF NOT EXISTS hosted_approvals (
    host_id TEXT PRIMARY KEY,
    state   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS queue (
    agent        TEXT NOT NULL,
    committee    TEXT NOT NULL,
    seq          INTEGER NOT NULL,
    event        TEXT NOT NULL,
    status       TEXT NOT NULL,
    session_id   TEXT,
    via          TEXT,
    enqueued_at  INTEGER NOT NULL,
    delivered_at INTEGER,
    redelivered  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent, committee, seq)
);
CREATE INDEX IF NOT EXISTS queue_by_status ON queue (status, agent, committee, seq);
CREATE TABLE IF NOT EXISTS dropped (
    agent     TEXT NOT NULL,
    committee TEXT NOT NULL,
    count     INTEGER NOT NULL,
    PRIMARY KEY (agent, committee)
);
CREATE TABLE IF NOT EXISTS verdicts (
    committee TEXT NOT NULL,
    root    INTEGER NOT NULL,
    verdict TEXT NOT NULL,
    PRIMARY KEY (committee, root)
);
CREATE TABLE IF NOT EXISTS person_seen (
    committee TEXT NOT NULL,
    root      INTEGER NOT NULL,
    seen      INTEGER NOT NULL,
    PRIMARY KEY (committee, root)
);
CREATE TABLE IF NOT EXISTS owners (
    committee  TEXT NOT NULL,
    root       INTEGER NOT NULL,
    agent      TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (committee, root)
);
CREATE TABLE IF NOT EXISTS hosted_paused (
    agent      TEXT PRIMARY KEY,
    committee  TEXT NOT NULL,
    paused_at  INTEGER NOT NULL,
    checkpoint INTEGER
);
CREATE TABLE IF NOT EXISTS hosted_stopped (
    agent      TEXT PRIMARY KEY,
    stopped_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS spawn_attempts (
    id      TEXT PRIMARY KEY,
    record  TEXT NOT NULL,
    expires INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS spawn_attempts_expiry ON spawn_attempts (expires);
CREATE TABLE IF NOT EXISTS replacements (
    old     TEXT PRIMARY KEY,
    record  TEXT NOT NULL,
    updated INTEGER NOT NULL
);

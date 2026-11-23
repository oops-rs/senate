use std::path::Path;

use rusqlite::{Connection, params};

pub const ID: &str = "1da2c199-0000-4000-a000-000000000001";
pub const SESSION: &str = "agy:1da2c199-0000-4000-a000-000000000001";

pub fn create(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA user_version=1;
         CREATE TABLE trajectory_meta(trajectory_id TEXT PRIMARY KEY, cascade_id TEXT,
             trajectory_type INTEGER, source INTEGER);
         CREATE TABLE steps(idx INTEGER PRIMARY KEY, step_type INTEGER NOT NULL DEFAULT 0,
             status INTEGER NOT NULL DEFAULT 0, has_subtrajectory NUMERIC NOT NULL DEFAULT false,
             metadata BLOB, error_details BLOB, permissions BLOB, task_details BLOB, render_info BLOB,
             step_payload BLOB, step_format INTEGER NOT NULL DEFAULT 0);
         CREATE INDEX idx_steps_status ON steps(status);
         CREATE INDEX idx_steps_step_type ON steps(step_type);
         CREATE TABLE gen_metadata(idx INTEGER PRIMARY KEY, data BLOB, size INTEGER NOT NULL DEFAULT 0);
         CREATE TABLE executor_metadata(idx INTEGER PRIMARY KEY, data BLOB);
         CREATE TABLE parent_references(idx INTEGER PRIMARY KEY, data BLOB);
         CREATE TABLE trajectory_metadata_blob(id TEXT PRIMARY KEY DEFAULT 'main', data BLOB);
         CREATE TABLE battle_mode_infos(idx INTEGER PRIMARY KEY, data BLOB);",
    ).unwrap();
    db.execute(
        "INSERT INTO trajectory_meta VALUES('trajectory', ?1, 4, 17)",
        [ID],
    )
    .unwrap();
    db
}

pub fn varint(mut n: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while n >= 128 {
        bytes.push((n as u8) | 128);
        n >>= 7;
    }
    bytes.push(n as u8);
    bytes
}

pub fn number(tag: u64, n: u64) -> Vec<u8> {
    [varint(tag << 3), varint(n)].concat()
}

pub fn field(tag: u64, s: impl AsRef<[u8]>) -> Vec<u8> {
    let bytes = s.as_ref();
    [
        varint(tag << 3 | 2),
        varint(bytes.len() as u64),
        bytes.to_vec(),
    ]
    .concat()
}

pub fn step(
    db: &Connection,
    idx: i64,
    kind: u64,
    status: u64,
    source: u64,
    call: Vec<u8>,
    payload: Vec<u8>,
) {
    let metadata = [
        field(1, number(1, 1_700_000_000 + idx as u64)),
        number(3, source),
        call,
    ]
    .concat();
    let payload = [number(1, kind), number(4, status), payload].concat();
    db.execute(
        "INSERT INTO steps(idx, step_type, status, metadata, step_payload) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(idx) DO UPDATE SET status=excluded.status,step_payload=excluded.step_payload",
        params![idx, kind as i64, status as i64, metadata, payload],
    ).unwrap();
}

pub fn user(db: &Connection, idx: i64, source: u64, text: &str) {
    step(
        db,
        idx,
        14,
        3,
        source,
        Vec::new(),
        field(19, field(3, field(1, text))),
    );
}

pub fn planner(db: &Connection, idx: i64, message: &str, reasoning: &str) {
    step(
        db,
        idx,
        15,
        3,
        2,
        Vec::new(),
        field(20, [field(1, message), field(3, reasoning)].concat()),
    );
}

pub fn tool(
    db: &Connection,
    idx: i64,
    status: u64,
    name: &str,
    args: &str,
    result: Option<Vec<u8>>,
) {
    let call = field(
        4,
        [
            field(1, format!("call-{idx}")),
            field(2, name),
            field(3, args),
        ]
        .concat(),
    );
    step(
        db,
        idx,
        132,
        status,
        2,
        call,
        field(140, result.map_or(vec![], |r| field(2, r))),
    );
}

pub fn shell_result(code: u64, output: &str) -> Vec<u8> {
    [
        field(1, output),
        field(
            6,
            [
                field(1, "type.googleapis.com/exa.cortex.core.tools.RunCommand"),
                field(2, [number(1, code), field(7, "/tmp/fixtures")].concat()),
            ]
            .concat(),
        ),
    ]
    .concat()
}

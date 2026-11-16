//! The committee log: one append-only table keyed by `(committee, seq)`,
//! plus the small side tables a restart needs — named cursors, outbound
//! mirror refs, and the latest bridge heartbeat per agent.
//!
//! [`LogStore`] is the seam. The server holds an `Arc<dyn LogStore>` and is the
//! only component that does; everything else reaches the log through
//! [`senate_core::Hub`]. Three implementations ship, and [`connect`] picks one
//! from a URL:
//!
//! - [`MemoryStore`] (`memory:`): a mutex over vectors, for tests and dev mode.
//! - [`PgStore`] (`postgres://...`): Postgres via sqlx with embedded
//!   migrations, for a server.
//! - [`SqliteStore`] (`sqlite:<path>`): one embedded on-disk file, for a
//!   developer machine. Its search is a plain substring scan.
//!
//! All three pass the same conformance suite (`tests/conformance.rs`).
//!
//! # Semantics every store keeps
//!
//! - **Sequence.** `seq` is per committee, starts at 1, and has no gaps: the
//!   n-th distinct event appended to a committee gets `Seq(n)`. Assignment is
//!   correct under concurrent appends to one committee, even though the server
//!   already serializes them through the committee's single writer.
//! - **Idempotence.** Appends are keyed on `(committee, id)`. Re-appending a
//!   stored id returns the stored event with `duplicate: true`, regardless of
//!   the new body, and never consumes a seq.
//! - **Storable events only.** An event whose `to`, `from` or `on_behalf_of`
//!   still holds `senator::<person>/*` ([`senate_core::NewEvent::is_resolved`]
//!   is false) is refused with [`LogError::Invalid`]; whoever accepts an event
//!   resolves the wildcard against the roster first.
//! - **Time.** `ts` is the store's clock at append, truncated to microseconds
//!   (Postgres `timestamptz` precision) so an appended event compares equal to
//!   the same event read back.
//! - **Cursors** are overwrite, not monotonic: `set_cursor` stores exactly the
//!   seq given, so a consumer can rewind. An unset cursor reads as `None`.
//!   A cursor belongs to a [`CursorOwner`]: the full path of one agent, or the
//!   hub. Two agents of one person never share a cursor.
//! - **Epoch.** Minted once when the log is created (a [`MemoryStore`] at
//!   construction, a [`PgStore`] database or a [`SqliteStore`] file on its
//!   first migration) and constant
//!   after that. It is a ULID string.
//! - **Mirror refs** are first-write-wins on `(committee, seq, platform,
//!   target)`: a second `put_mirror` for the same key is a no-op, so a retried
//!   post can never re-point a mirror, while one event may have several
//!   copies under different targets. When several copies claim the same
//!   platform message, [`LogStore::mirror_of_message`] returns the one with
//!   the lowest `(committee, seq, target)` — normally the thread root.
//! - **Queries.** [`LogStore::thread`], [`LogStore::bindings`] and
//!   [`LogStore::search`] return exactly what the scanning defaults of the
//!   same-named [`senate_core::Hub`] methods return; stores answer them from
//!   an index instead of a scan. The one allowed difference: [`PgStore`]'s
//!   search appends near matches (`pg_trgm` word similarity ≥
//!   [`NEAR_MATCH_SIMILARITY`]) after the exact ones when there is room.
//! - **Heartbeats** keep only the latest per `(committee, agent)`, the agent
//!   by its full [`senate_core::SenatorPath`].

mod error;
mod memory;
mod pg;
mod sqlite;
mod store;

pub use error::LogError;
pub use memory::MemoryStore;
pub use pg::{NEAR_MATCH_SIMILARITY, PgStore};
pub use sqlite::SqliteStore;
pub use store::{CursorOwner, HeartbeatRecord, LogStore, MEMORY_URL, SQLITE_PREFIX, connect, now};

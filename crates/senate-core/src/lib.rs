//! Senate contracts.
//!
//! Everything that crosses a crate or process boundary is defined here, once:
//!
//! - [`ids`]: committee ids, name segments, `seq`, event ids.
//! - [`senator`]: [`SenatorPath`], a person (`wendell`) or one of their
//!   agents at any depth (`wendell/claude`, `wendell/claude/reviewer`).
//! - [`address`]: `senator::<path>`, `senator::<person>/*`, `speaker`,
//!   `committee`, `guest::<platform>/<id>`, as stored in an event's fields.
//! - [`mention`]: the same ids as written in running text, in brackets:
//!   `[senator::wendell/claude]`, `[speaker::wendell/claude]`.
//! - [`roster`]: who sits in one Senate, and the rules that hold for it.
//! - [`event`]: the single envelope of a committee's log.
//!
//! # The model
//!
//! A **Senate** is one level of deliberation. Its **senators** are people and
//! agents. A person is a *senior* senator and is global: the same senator in
//! every Senate. An agent is a *junior* senator, named under its person. The
//! **Speaker** presides and is always backed by a model.
//!
//! Senates nest. An agent seated in one Senate may preside, as Speaker, over
//! a Senate of its own subagents one level down, and speaks upward in its own
//! name for what happens there. Only the top Senate's Speaker is not a
//! senator.
//!
//! Work is divided into **committees**, each with its own append-only log.
//!
//! # Wire compatibility
//!
//! Fields are only ever added, never renamed or repurposed; every optional
//! field is `#[serde(default)]` so an older reader accepts a newer writer,
//! and unknown kinds and address forms round-trip through [`Kind::Other`] and
//! [`Address::Other`].

pub mod address;
pub mod event;
pub mod ids;
pub mod mention;
pub mod roster;
pub mod senator;

pub use address::{Address, AddressParseError, Guest, GuestError, MAX_GUEST_ID_LEN};
pub use event::{Event, Kind, Mirror, NewEvent, Refs, Source};
pub use ids::{CommitteeId, EventId, IdError, Name, Seq};
pub use mention::{Found, Mention, MentionParseError};
pub use roster::{Account, AgentOf, DelegationPresence, Roster, RosterError, Senior};
pub use senator::{MAX_AGENT_DEPTH, SenatorPath, SenatorPathError};

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
//! - [`roster`]: who sits in one Senate, the rules that hold for it, who may
//!   act as whom, and what "this person's agent" resolves to.
//! - [`locale`]: the language a text for people is rendered in.
//! - [`event`]: the single envelope of a committee's log, its payloads and
//!   its shared `meta` keys.
//! - [`api`]: the server's HTTP/SSE wire types, shared by server and bridge.
//! - [`hub`]: the in-process seam server-side components (the Speaker, a
//!   gateway) use to append to and observe the log without going over HTTP.
//! - [`surface`]: the seam between the Speaker and a chat-platform gateway.
//! - [`brief`]: the committee brief, derived from the log.
//! - [`continuity`]: checkpoints, presence and leases, kept per agent.
//! - [`docket`]: what awaits a Speaker held by a senator: picks, messages
//!   addressed to it, questions to away agents.
//! - [`task`]: task threads: status, triage, and the board derived from
//!   them.
//! - [`work`]: the Speaker's work runs, branch naming, and reassignment.
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
//! name for what happens there. Who holds a Senate's Speaker's seat is data
//! ([`SpeakerSeat`]): the built-in Speaker (a top Senate only), or a senator,
//! by default the one presiding over a nested Senate.
//!
//! Work is divided into **committees**, each with its own append-only log.
//!
//! Two consequences run through every module here. An agent is a path, and
//! a person may have several, so nothing is keyed by "a person's agent":
//! presence, checkpoints, leases and tasks are keyed by [`SenatorPath`], and
//! getting from a person to an agent is a roster lookup that may find one,
//! several or none ([`Roster::agent_of`]). And a contract that describes a
//! Senate says which one by its presider ([`Roster::presider`], `None` for
//! the top), so nothing assumes there is only one level.
//!
//! # Languages
//!
//! Chinese and English are both first-class. Whatever is stored or sent is
//! the same in every language; a function that writes text for people takes
//! a [`Locale`]. See [`locale`].
//!
//! # Wire compatibility
//!
//! Fields are only ever added, never renamed or repurposed; every optional
//! field is `#[serde(default)]` so an older reader accepts a newer writer,
//! and unknown kinds and address forms round-trip through [`Kind::Other`] and
//! [`Address::Other`].

pub mod address;
pub mod api;
pub mod brief;
pub mod continuity;
pub mod docket;
pub mod event;
pub mod hub;
pub mod ids;
pub mod locale;
pub mod mention;
pub mod roster;
pub mod senator;
pub mod surface;
pub mod task;
pub mod work;

pub use address::{Address, AddressParseError, Guest, GuestError, MAX_GUEST_ID_LEN};
pub use brief::{Brief, BriefBuilder};
pub use continuity::{
    AgentPresence, CheckpointFiles, CheckpointPayload, CheckpointRecord, CheckpointTrigger,
    Checkpoints, PresenceBoard, PresencePayload, PresenceState, presence_event,
};
pub use docket::{
    Awaiting, CheckpointFacts, DocketItem, DocketPage, PickPayload, SeatFacts, SpeakerDocket,
    awaits_speaker,
};
pub use event::{
    BindingAction, BindingPayload, Event, GroupRole, Kind, Mirror, NewEvent, PROPOSAL_DECISION,
    Refs, Source, StatusState, is_companion, is_provisional, is_takeover, meta,
    thread_participants,
};
pub use hub::{
    Appended, Hub, HubError, MirrorRef, ReassignRefused, reassign_event, reassign_transition,
};
pub use ids::{CommitteeId, EventId, IdError, Name, Seq};
pub use locale::{Locale, LocaleParseError};
pub use mention::{Found, Mention, MentionParseError};
pub use roster::{
    Account, AgentOf, DelegationPresence, ResolveError, Roster, RosterConfig, RosterError, Senior,
    SpeakerSeat,
};
pub use senator::{MAX_AGENT_DEPTH, SenatorPath, SenatorPathError};
pub use surface::{
    AdoptedGroup, Card, ChatSurface, CreatedGroup, NewGroup, PostTarget, Posted, SurfaceError,
    UnboundMention,
};
pub use task::{
    Link, OutputLevel, StatusPayload, Step, TaskBoard, TaskOwner, ThreadSummary, TriagePayload,
    TriageVerdict, needs_triage,
};
pub use work::{BranchNaming, DEFAULT_BRANCH_PREFIX, WorkRun, WorkState, is_work_root};

/// What every rendered text is held to in this crate's tests.
#[cfg(test)]
pub(crate) mod testing {
    /// Panics when `text` uses the predecessor's vocabulary: its address
    /// prefixes, or its words for the Speaker and for a committee. A git
    /// branch such as `feature/refund-v2/alice` is a developer's convention,
    /// not that vocabulary, so `feature/` is let through.
    pub(crate) fn assert_senate_vocabulary(text: &str) {
        let lower = text.to_lowercase();
        for word in ["user:", "agent:", "steward"] {
            assert!(!lower.contains(word), "{word:?} in {text:?}");
        }
        assert!(
            !lower.replace("feature/", "").contains("feature"),
            "\"feature\" in {text:?}"
        );
    }

    #[test]
    #[should_panic(expected = "feature")]
    fn the_vocabulary_check_catches_the_old_word_for_a_committee() {
        assert_senate_vocabulary("on feature/refund-v2/alice for this feature");
    }

    #[test]
    fn the_vocabulary_check_lets_a_branch_name_through() {
        assert_senate_vocabulary("[senator::alice/claude] on feature/refund-v2/alice");
    }
}

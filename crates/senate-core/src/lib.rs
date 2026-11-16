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
//! - [`locale`]: the language a text for people is rendered in.
//! - [`continuity`]: checkpoints, presence and leases, kept per agent.
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
pub mod brief;
pub mod continuity;
pub mod event;
pub mod ids;
pub mod locale;
pub mod mention;
pub mod roster;
pub mod senator;
pub mod task;
pub mod work;

pub use address::{Address, AddressParseError, Guest, GuestError, MAX_GUEST_ID_LEN};
pub use brief::{Brief, BriefBuilder};
pub use continuity::{
    AgentPresence, CheckpointFiles, CheckpointPayload, CheckpointRecord, CheckpointTrigger,
    Checkpoints, PresenceBoard, PresencePayload, PresenceState, presence_event,
};
pub use event::{
    BindingAction, BindingPayload, Event, GroupRole, Kind, Mirror, NewEvent, PROPOSAL_DECISION,
    Refs, Source, StatusState, is_companion, is_provisional, is_takeover, meta,
    thread_participants,
};
pub use ids::{CommitteeId, EventId, IdError, Name, Seq};
pub use locale::{Locale, LocaleParseError};
pub use mention::{Found, Mention, MentionParseError};
pub use roster::{
    Account, AgentOf, DelegationPresence, ResolveError, Roster, RosterConfig, RosterError, Senior,
};
pub use senator::{MAX_AGENT_DEPTH, SenatorPath, SenatorPathError};
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

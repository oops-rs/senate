//! Senate contracts.
//!
//! Everything that crosses a crate or process boundary is defined here, once:
//!
//! - [`ids`]: committee ids, name segments, `seq`, event ids.
//! - [`senator`]: [`SenatorName`], one flat name scoped to a Senate: a
//!   person (`wendell`) or an agent (`claude`, `codex`).
//! - [`senate`]: Senate names and [`SenatePath`] (`acme/north-desk`); only
//!   Senates nest.
//! - [`address`]: `senator::<name>`, `senator::*`, `speaker`, `committee`,
//!   `guest::<platform>/<id>`, as stored in an event's fields.
//! - [`senator_id`]: who a senator is: `senator::wendell`,
//!   `/acme::senator::wendell` (a Senate path in an id starts with `/`).
//! - [`locator`]: qualified names, read from the right
//!   (`acme/north-desk::ibot::senator::codex`, `ibot::speaker`), naming a
//!   seat or a Speaker from anywhere.
//! - [`mention`]: the same ids as written in running text, in brackets:
//!   `[senator::claude]`, `[speaker::claude]`, `[ibot::senator::codex]`.
//! - [`roster`]: who sits in one Senate, the rules that hold for it, who may
//!   act as whom, and what "whichever agent" resolves to.
//! - [`locale`]: the language a text for people is rendered in.
//! - [`event`]: the single envelope of a committee's log, its payloads and
//!   its shared `meta` keys.
//! - [`api`]: the server's HTTP/SSE wire types, shared by server and bridge.
//! - [`hub`]: the in-process seam server-side components (the Speaker, a
//!   gateway) use to append to and observe the log without going over HTTP.
//! - [`surface`]: the seam between the Speaker and a chat-platform gateway.
//! - [`brief`]: the committee brief, derived from the log.
//! - [`continuity`]: checkpoints, presence and leases, kept per agent.
//! - [`duty`]: what each seat does on a committee, folded from the log.
//! - [`docket`]: what awaits a Speaker held by a senator: picks, messages
//!   addressed to it, questions to away agents.
//! - [`task`]: task threads: status, triage, and the board derived from
//!   them.
//! - [`work`]: the Speaker's work runs, branch naming, and reassignment.
//! - [`flatten`]: converting what was stored when senators were paths
//!   (`senator::wendell/claude`) to flat names, once.
//!
//! # The model
//!
//! Only Senates nest. Senators and committees are flat, and both are scoped
//! to one Senate:
//!
//! | Thing | Scope | Shape | Example |
//! | --- | --- | --- | --- |
//! | Senate | its parent Senate | a path of names, parent first | `acme/north-desk` |
//! | Committee | one Senate | flat id | `ibot` |
//! | Senator | one Senate | flat name | `wendell`, `codex` |
//! | Seat | one committee | committee × senator | `ibot::senator::codex` |
//!
//! A **senator** is a person or an agent. No path is inside a senator's
//! name: nothing about who owns it or where it sits. Whether a name is a
//! person's or an agent's is the roster's to say. A **seat** is a senator
//! on a committee, and messages only ever go to seats: each seat has its
//! own sessions, inbox, cursors, presence and checkpoints, so presence,
//! checkpoints, leases and tasks are keyed by committee and
//! [`SenatorName`].
//!
//! The **Speaker** presides and is always backed by a model. Who holds the
//! Speaker's seat is data ([`SpeakerSeat`]): the built-in Speaker, a
//! senator, or nobody.
//!
//! A qualified name ([`Locator`]) says where a seat is; the parts it leaves
//! out come from where the writer stands, nearest first.
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
pub mod chair;
pub mod committee_seat;
pub mod continuity;
pub mod docket;
pub mod duty;
pub mod event;
pub mod flatten;
pub mod hub;
pub mod ids;
pub mod locale;
pub mod locator;
pub mod mention;
pub mod roster;
pub mod secretary_work;
pub mod senate;
pub mod senator;
pub mod senator_id;
pub mod surface;
pub mod task;
pub mod work;

pub use address::{Address, AddressParseError, Guest, GuestError, MAX_GUEST_ID_LEN};
pub use api::{ChildSeat, ChildSeats, ChildSenate};
pub use brief::{Brief, BriefBuilder};
pub use chair::{ChairFold, ChairPayload, ChairSource};
pub use committee_seat::{CommitteeSeatAction, CommitteeSeatChange, CommitteeSeats};
pub use continuity::{
    AgentPresence, CheckpointFiles, CheckpointPayload, CheckpointRecord, CheckpointTrigger,
    Checkpoints, PresenceBoard, PresencePayload, PresenceState, presence_event,
};
pub use docket::{
    Awaiting, CheckpointFacts, DocketItem, DocketPage, PickPayload, SeatFacts, SpeakerDocket,
    awaits_speaker,
};
pub use duty::{DutyFold, DutyPayload};
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
pub use locator::{Locator, LocatorParseError, Origin};
pub use mention::{Found, Mention, MentionParseError};
pub use roster::{
    Account, AgentOf, DEFAULT_SENATE_NAME, DelegationPresence, Office, People, Principal,
    PrincipalKind, ResolveError, Roster, RosterConfig, RosterError, SeatedAgent, SenatorKind,
    SpeakerName, SpeakerSeat,
};
pub use senate::{KEYWORDS, KeywordError, SenatePath, SenatePathError, check_not_keyword};
pub use senator::{SenatorName, SenatorNameError};
pub use senator_id::{SenatorId, SenatorIdParseError};
pub use surface::{
    AdoptedGroup, Card, ChatSurface, CreatedGroup, NewGroup, PostTarget, Posted, SurfaceError,
    UnboundMention,
};
pub use task::{
    Attention, Directed, Escalation, Link, OutputLevel, PersonWait, StatusPayload, Step, TaskBoard,
    TaskOwner, ThreadSummary, TriagePayload, TriageVerdict, WaitsOn, attention, needs_triage,
    seen_by_author,
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
        assert_senate_vocabulary("[senator::claude] on feature/refund-v2/alice");
    }
}

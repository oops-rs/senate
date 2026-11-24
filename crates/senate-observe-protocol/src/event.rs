use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Address, Approval, ApprovalDecision, ApprovalTool, Committee, Cursor, DeskItem, HoldReason,
    LinkState, MessageKind, Navigation, Project, RoutePhase, Senate, SenateLink, Senator,
    SenatorState, Seq, Thread, ThreadState, V1, WaitsOn, Windows,
};

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct EventFrame {
    pub schema_version: V1,
    pub cursor: Cursor,
    pub at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committee_id: Option<String>,
    pub link: Option<String>,
    #[serde(flatten)]
    pub event: Event,
}

/// Read this header before decoding a typed event. Unknown kinds may still
/// advance a client's cursor. A malformed known event should force resync.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventHeader {
    pub schema_version: V1,
    pub cursor: Option<Cursor>,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ResetFrame {
    pub schema_version: V1,
    pub at: DateTime<Utc>,
    pub kind: ResetKind,
    pub data: ResetData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub enum ResetKind {
    #[serde(rename = "stream.reset")]
    StreamReset,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ResetData {
    pub reason: ResetReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    Overrun,
    SourceReset,
    PolicyChanged,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum Event {
    #[serde(rename = "state.patch")]
    StatePatch(Box<StatePatch>),
    #[serde(rename = "message.posted")]
    MessagePosted(MessagePosted),
    #[serde(rename = "message.line_published")]
    MessageLinePublished(MessageLinePublished),
    #[serde(rename = "message.routed")]
    MessageRouted(MessageRouted),
    #[serde(rename = "thread.state_changed")]
    ThreadStateChanged(ThreadStateChanged),
    #[serde(rename = "senator.state_changed")]
    SenatorStateChanged(SenatorStateChanged),
    #[serde(rename = "senator.joined")]
    SenatorJoined(SenatorRef),
    #[serde(rename = "senator.left")]
    SenatorLeft(SenatorRef),
    #[serde(rename = "senator.started")]
    SenatorStarted(SenatorRef),
    #[serde(rename = "senator.stopped")]
    SenatorStopped(SenatorRef),
    #[serde(rename = "senator.paused")]
    SenatorPaused(SenatorRef),
    #[serde(rename = "senator.resumed")]
    SenatorResumed(SenatorRef),
    #[serde(rename = "committee.chair_changed")]
    CommitteeChairChanged(ChairChanged),
    #[serde(rename = "committee.created")]
    CommitteeCreated(CommitteeRef),
    #[serde(rename = "committee.archived")]
    CommitteeArchived(CommitteeArchived),
    #[serde(rename = "approval.asked")]
    ApprovalAsked(ApprovalAsked),
    #[serde(rename = "approval.decided")]
    ApprovalDecided(ApprovalDecided),
    #[serde(rename = "viewer.attached")]
    ViewerAttached(SenatorRef),
    #[serde(rename = "viewer.detached")]
    ViewerDetached(SenatorRef),
    #[serde(rename = "link.changed")]
    LinkChanged(LinkChanged),
    #[serde(rename = "card.changed")]
    CardChanged(CardChanged),
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct MessagePosted {
    pub seq: Seq,
    pub message_kind: MessageKind,
    pub from: Address,
    pub to: Vec<Address>,
    pub root_seq: Option<Seq>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 80))]
    pub line: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct MessageLinePublished {
    pub seq: Seq,
    #[schemars(length(max = 80))]
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct MessageRouted {
    pub route_id: String,
    pub seq: Option<Seq>,
    pub target: Option<Address>,
    pub phase: RoutePhase,
    pub reason: Option<HoldReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ThreadStateChanged {
    pub root_seq: Seq,
    pub previous_state: ThreadState,
    pub state: ThreadState,
    pub waits_on: WaitsOn,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct SenatorStateChanged {
    pub senator_id: String,
    pub previous_state: SenatorState,
    pub state: SenatorState,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct SenatorRef {
    pub senator_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct CommitteeRef {
    pub committee_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ChairChanged {
    pub previous_chair_id: Option<String>,
    pub chair_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct CommitteeArchived {
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ApprovalAsked {
    pub id: String,
    pub senator_id: String,
    pub tool_kind: ApprovalTool,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ApprovalDecided {
    pub id: String,
    pub tool_kind: ApprovalTool,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct LinkChanged {
    pub id: String,
    pub previous_state: LinkState,
    pub state: LinkState,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct CardChanged {
    pub subject: CardSubject,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardSubject {
    Committee {
        committee_id: String,
    },
    Senator {
        committee_id: String,
        senator_id: String,
    },
    Person {
        person_id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct StatePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upsert: Option<Upserts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Removals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<Replacements>,
}

macro_rules! row_changes {
    ($name:ident { $($field:ident: $row:ty),+ $(,)? }) => {
        #[derive(Debug, Clone, Default, PartialEq, JsonSchema, Serialize, Deserialize)]
        pub struct $name {
            $(
                #[serde(default, skip_serializing_if = "Vec::is_empty")]
                pub $field: Vec<$row>,
            )+
        }
    };
}

row_changes!(Upserts {
    links: SenateLink,
    committees: Committee,
    projects: Project,
    senators: Senator,
    threads: Thread,
    desk_items: DeskItem,
    approvals: Approval,
});

row_changes!(Removals {
    links: IdKey,
    committees: IdKey,
    projects: IdKey,
    senators: SeatKey,
    threads: ThreadKey,
    desk_items: ThreadKey,
    approvals: IdKey,
});

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct IdKey {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct SeatKey {
    pub committee_id: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ThreadKey {
    pub committee_id: String,
    pub root_seq: Seq,
}

#[derive(Debug, Clone, Default, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Replacements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senate: Option<Senate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Windows>,
    /// All individual destinations may be null when clearing the navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<Navigation>,
}

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{Cursor, Seq, V1};

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: V1,
    pub capabilities: Vec<String>,
    pub as_of: DateTime<Utc>,
    pub cursor: Cursor,
    pub senate: Senate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation: Option<Navigation>,
    pub links: Vec<SenateLink>,
    pub committees: Vec<Committee>,
    pub projects: Vec<Project>,
    pub senators: Vec<Senator>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<RetiredSenator>,
    pub threads: Vec<Thread>,
    pub desk_items: Vec<DeskItem>,
    pub approvals: Vec<Approval>,
    pub windows: Windows,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Senate {
    pub id: String,
    pub name: String,
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<Person>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desk_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub desk_workers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub name: String,
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Navigation {
    pub desk: Option<String>,
    pub approvals: Option<String>,
    pub inbox: Option<String>,
    pub history: Option<String>,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct SenateLink {
    pub id: String,
    pub kind: LinkKind,
    pub name: String,
    pub state: LinkState,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Card {
    pub revision: String,
    pub updated_at: DateTime<Utc>,
    pub value: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Committee {
    pub id: String,
    pub name: String,
    pub chair_id: Option<String>,
    pub archived: bool,
    pub project_id: Option<String>,
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Senator {
    pub id: String,
    pub committee_id: String,
    pub alias: Option<String>,
    pub runtime: Option<String>,
    pub kind: SenatorKind,
    pub state: SenatorState,
    pub last_active_at: Option<DateTime<Utc>>,
    pub hosted: Hosting,
    pub viewer_attached: Option<bool>,
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct RetiredSenator {
    pub id: String,
    pub committee_id: String,
    pub alias: Option<String>,
    pub runtime: Option<String>,
    pub last_active_at: Option<DateTime<Utc>>,
    pub retired_at: DateTime<Utc>,
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<Card>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Thread {
    pub committee_id: String,
    pub root_seq: Seq,
    pub kind: MessageKind,
    pub opener: Address,
    pub addressees: Vec<Address>,
    pub state: ThreadState,
    pub waits_on: WaitsOn,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct DeskItem {
    pub committee_id: String,
    pub root_seq: Seq,
    pub kind: MessageKind,
    pub awaiting: Awaiting,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub committee_id: String,
    pub senator_id: String,
    pub tool_kind: ApprovalTool,
    pub asked_at: DateTime<Utc>,
    pub link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Windows {
    pub open_threads_per_committee: u16,
    pub settled_threads_per_committee: u16,
    pub desk_items_per_committee: u16,
    pub truncated: Vec<Truncation>,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Truncation {
    pub committee_id: String,
    pub open_threads: bool,
    pub settled_threads: bool,
    pub desk_items: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Address {
    Senator {
        id: String,
    },
    Office,
    External,
    #[serde(other)]
    Unknown,
}

macro_rules! wire_enum {
    ($name:ident { $($variant:ident),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant,)*
            #[serde(other)]
            Unknown,
        }
    };
}

wire_enum!(LinkKind {
    Parent,
    Child,
    PairedMachine
});
wire_enum!(LinkState { Up, Down });
wire_enum!(SenatorKind { Person, Agent });
wire_enum!(Hosting { Here, Elsewhere });
wire_enum!(SenatorState {
    Working,
    Idle,
    WaitingOnPerson,
    Paused,
    Stopped,
    Away
});
wire_enum!(MessageKind {
    Question,
    Request,
    Answer,
    Note,
    Decision
});
wire_enum!(ThreadState {
    Processing,
    Active,
    Suspended,
    Archived
});
wire_enum!(WaitsOn {
    Agents,
    Person,
    None
});
wire_enum!(Awaiting {
    Pick,
    Addressed,
    Away,
    ChildOffline
});
wire_enum!(ApprovalTool {
    Command,
    Edit,
    Other
});
wire_enum!(RoutePhase {
    Received,
    Filed,
    Queued,
    HeldForHook,
    Released,
    Delivered
});
wire_enum!(HoldReason {
    AwaitingTriage,
    Busy,
    Paused,
    Offline,
    NoPush,
    Retry
});
wire_enum!(ApprovalDecision {
    Allowed,
    Denied,
    Dismissed
});

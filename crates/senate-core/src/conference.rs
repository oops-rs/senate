//! Cross-committee multi-party `Conference` contracts, room metadata, and
//! deterministic anti-loop [`FloorState`] rules.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use ulid::Ulid;

use crate::address::Address;
use crate::event::{Event, Kind};
use crate::ids::{CommitteeId, IdError, Seq};
use crate::locator::Locator;

/// Prefix shared by every isolated conference stream id (`conf-<ulid>`).
pub const CONFERENCE_PREFIX: &str = "conf-";
/// Maximum topic length for a conference.
pub const MAX_TOPIC_CHARS: usize = 160;
/// Maximum decisions in a conference report.
pub const MAX_REPORT_DECISIONS: usize = 32;
/// Maximum action items in a conference report.
pub const MAX_REPORT_ACTION_ITEMS: usize = 32;

/// Unique identifier for a conference (`conf-<ulid>`).
///
/// Every `ConferenceId` is also a valid [`CommitteeId`] slug so the server can
/// back each conference room with an isolated append-only log stream without
/// altering storage schemas, while filtering `conf-*` out of regular committee
/// endpoints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConferenceId(String);

impl ConferenceId {
    /// Generate a fresh `conf-<ulid>` identifier.
    pub fn fresh() -> Self {
        Self(format!(
            "{CONFERENCE_PREFIX}{}",
            Ulid::new().to_string().to_ascii_lowercase()
        ))
    }

    /// Validate and construct a [`ConferenceId`].
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        let rest = value.strip_prefix(CONFERENCE_PREFIX);
        let valid_rest = rest.is_some_and(|tail| !tail.is_empty() && tail.len() <= 58);
        if valid_rest && CommitteeId::new(value.clone()).is_ok() {
            Ok(Self(value))
        } else {
            Err(IdError::InvalidSlug {
                kind: "conference id",
                value,
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Underlying isolated log stream identifier for this conference.
    pub fn as_committee_id(&self) -> CommitteeId {
        CommitteeId::new(self.0.clone()).expect("ConferenceId is always a valid CommitteeId")
    }
}

/// Returns `true` if `committee` is an isolated conference room stream
/// (`conf-*`) rather than a regular Senate committee.
pub fn is_conference_stream(committee: &CommitteeId) -> bool {
    committee.as_str().starts_with(CONFERENCE_PREFIX)
}

impl fmt::Display for ConferenceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ConferenceId {
    type Err = IdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for ConferenceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ConferenceId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ConferenceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// Lifecycle state of a conference room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConferenceState {
    Convening,
    InSession,
    Adjourned,
}

/// Attendance state of a single invited seat in a conference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SeatStatus {
    Pending,
    Absent,
    Seated {
        participant: Address,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegated_by: Option<Address>,
        #[serde(default)]
        ephemeral: bool,
    },
}

/// An invited seat in a conference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitedSeat {
    pub target: Locator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee: Option<CommitteeId>,
    pub status: SeatStatus,
}

/// Optional originating task thread linked to a conference for write-back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedThread {
    pub committee: CommitteeId,
    pub root: Seq,
}

/// Follow-up action item recorded when a conference adjourns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceActionItem {
    #[serde(alias = "title", alias = "summary")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee: Option<CommitteeId>,
}

/// Structured conclusion published when a conference adjourns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceReport {
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub action_items: Vec<ConferenceActionItem>,
}

impl ConferenceReport {
    /// Validate report invariants before adjourning.
    pub fn check(&self) -> Result<(), String> {
        if self.summary.trim().is_empty() {
            return Err("conference report summary cannot be empty".into());
        }
        if self.decisions.len() > MAX_REPORT_DECISIONS {
            return Err(format!(
                "conference report may contain at most {MAX_REPORT_DECISIONS} decisions"
            ));
        }
        if self.action_items.len() > MAX_REPORT_ACTION_ITEMS {
            return Err(format!(
                "conference report may contain at most {MAX_REPORT_ACTION_ITEMS} action items"
            ));
        }
        for item in &self.action_items {
            if item.description.trim().is_empty() {
                return Err("conference action item description cannot be empty".into());
            }
        }
        Ok(())
    }
}

/// Invitation metadata written to a committee thread or chair inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceInvitePayload {
    pub conference_id: ConferenceId,
    pub topic: String,
    pub convener: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Locator>,
    #[serde(default)]
    pub linked_threads: Vec<LinkedThread>,
}

/// Report write-back metadata written to participating committees and linked threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceReportPayload {
    pub conference_id: ConferenceId,
    pub topic: String,
    pub convener: Address,
    #[serde(default)]
    pub participants: Vec<Address>,
    pub report: ConferenceReport,
    #[serde(default)]
    pub ephemeral_delegates: Vec<Address>,
}

/// Deterministic anti-loop floor control for multi-party conferences.
///
/// A human post opens a fresh `round`, clears `spoken`, and resets
/// `agent_hops_remaining` to `1`. Explicit `@mention`s restrict delivery to
/// mentioned seated agents; unmentioned human posts grant at most one turn per
/// seated agent. Agent posts never re-trigger other agents unless an agent
/// explicitly `@mention`s an un-spoken seated agent while
/// `agent_hops_remaining > 0`, which consumes one hop and then forces silence
/// until a human speaks again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorState {
    pub round: u64,
    #[serde(default)]
    pub addressed: Vec<Address>,
    #[serde(default)]
    pub spoken: BTreeSet<Address>,
    pub agent_hops_remaining: u8,
}

impl Default for FloorState {
    fn default() -> Self {
        Self {
            round: 0,
            addressed: Vec::new(),
            spoken: BTreeSet::new(),
            agent_hops_remaining: 1,
        }
    }
}

impl FloorState {
    /// Advance floor state for a message from `from` and return the subset of
    /// `seated_agents` that should receive a turn push.
    pub fn advance(
        &mut self,
        from: &Address,
        from_is_human: bool,
        mentioned_seats: &[Address],
        seated_agents: &[Address],
    ) -> Vec<Address> {
        let mut eligible_mentions = Vec::new();
        for addr in mentioned_seats {
            if addr != from && seated_agents.contains(addr) && !eligible_mentions.contains(addr) {
                eligible_mentions.push(addr.clone());
            }
        }

        if from_is_human {
            self.round = self.round.saturating_add(1);
            self.spoken.clear();
            self.agent_hops_remaining = 1;
            self.addressed = eligible_mentions;
            if !self.addressed.is_empty() {
                return self.addressed.clone();
            }
            let mut broadcast = Vec::new();
            for agent in seated_agents {
                if agent != from && !broadcast.contains(agent) {
                    broadcast.push(agent.clone());
                }
            }
            return broadcast;
        }

        self.spoken.insert(from.clone());
        if self.agent_hops_remaining == 0 {
            return Vec::new();
        }
        for target in eligible_mentions {
            if !self.spoken.contains(&target) {
                self.agent_hops_remaining = self.agent_hops_remaining.saturating_sub(1);
                return vec![target];
            }
        }
        Vec::new()
    }
}

/// Seat invite entry in [`CreateConferenceRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceSeatInput {
    pub target: Locator,
}

/// `POST /v1/conferences` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateConferenceRequest {
    pub topic: String,
    #[serde(default)]
    pub seats: Vec<ConferenceSeatInput>,
    #[serde(default)]
    pub linked_threads: Vec<LinkedThread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
}

/// `POST /v1/conferences/{id}/rsvp` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConferenceRsvpAction {
    Attend {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Locator>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        participant: Option<Address>,
    },
    Delegate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Locator>,
        senator: Address,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegated_by: Option<Address>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        brief: Option<String>,
        #[serde(default)]
        ephemeral: bool,
    },
}

/// `POST /v1/conferences/{id}/events` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostConferenceEventRequest {
    pub from: Address,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub to: Vec<Address>,
    #[serde(default)]
    pub meta: Map<String, Value>,
}

/// `POST /v1/conferences/{id}/events` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostConferenceEventResponse {
    pub event: Event,
    #[serde(default)]
    pub duplicate: bool,
    pub floor: FloorState,
    #[serde(default)]
    pub push_targets: Vec<Address>,
}

/// `POST /v1/conferences/{id}/adjourn` request payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjournConferenceRequest {
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub action_items: Vec<ConferenceActionItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Address>,
}

/// Summary view of a conference returned by list and mutation endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceSummary {
    pub id: ConferenceId,
    pub topic: String,
    pub convener: Address,
    pub state: ConferenceState,
    pub seats: Vec<InvitedSeat>,
    #[serde(default)]
    pub linked_threads: Vec<LinkedThread>,
    pub floor: FloorState,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adjourned_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ConferenceReport>,
}

/// `GET /v1/conferences` response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConferenceList {
    #[serde(default)]
    pub conferences: Vec<ConferenceSummary>,
}

/// `GET /v1/conferences/{id}` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConferenceDetail {
    #[serde(flatten)]
    pub summary: ConferenceSummary,
    #[serde(default)]
    pub events: Vec<Event>,
}

pub fn conferences_path() -> &'static str {
    "/v1/conferences"
}

pub fn conference_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}")
}

pub fn conference_rsvp_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}/rsvp")
}

pub fn conference_start_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}/start")
}

pub fn conference_events_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}/events")
}

pub fn conference_stream_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}/stream")
}

pub fn conference_adjourn_path(id: &ConferenceId) -> String {
    format!("/v1/conferences/{id}/adjourn")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SenatorName;

    fn agent(name: &str) -> Address {
        Address::Senator(name.parse::<SenatorName>().expect("valid agent"))
    }

    #[test]
    fn conference_id_round_trips_as_committee_stream() {
        let id = ConferenceId::fresh();
        assert!(id.as_str().starts_with(CONFERENCE_PREFIX));
        let stream = id.as_committee_id();
        assert!(is_conference_stream(&stream));

        let regular = CommitteeId::new("refund-v2").unwrap();
        assert!(!is_conference_stream(&regular));
        assert!(ConferenceId::new("refund-v2").is_err());
        assert!(ConferenceId::new("conf-").is_err());
    }

    #[test]
    fn floor_state_enforces_human_rounds_and_agent_hop_budget() {
        let human = agent("wendell");
        let agent_a = agent("codex");
        let agent_b = agent("claude");
        let seated = vec![agent_a.clone(), agent_b.clone()];
        let mut floor = FloorState::default();

        // Human unmentioned message opens round 1 and yields <= 1 turn per seated agent.
        let pushed = floor.advance(&human, true, &[], &seated);
        assert_eq!(floor.round, 1);
        assert_eq!(floor.agent_hops_remaining, 1);
        assert_eq!(pushed, vec![agent_a.clone(), agent_b.clone()]);

        // Agent A replies without @mention -> zero follow-up pushes.
        let pushed = floor.advance(&agent_a, false, &[], &seated);
        assert!(pushed.is_empty());
        assert!(floor.spoken.contains(&agent_a));

        // Agent A could instead @mention un-spoken Agent B -> consumes the single hop.
        let pushed = floor.advance(&agent_a, false, std::slice::from_ref(&agent_b), &seated);
        assert_eq!(pushed, vec![agent_b.clone()]);
        assert_eq!(floor.agent_hops_remaining, 0);

        // Agent B replies and tries to @mention another -> suppressed until human speaks.
        let agent_c = agent("gemini");
        let seated_three = vec![agent_a.clone(), agent_b.clone(), agent_c.clone()];
        let pushed = floor.advance(
            &agent_b,
            false,
            std::slice::from_ref(&agent_c),
            &seated_three,
        );
        assert!(pushed.is_empty());

        // Human speaks again with explicit @mention -> opens round 2 and pushes only mentioned.
        let pushed = floor.advance(&human, true, std::slice::from_ref(&agent_c), &seated_three);
        assert_eq!(floor.round, 2);
        assert_eq!(floor.agent_hops_remaining, 1);
        assert!(floor.spoken.is_empty());
        assert_eq!(pushed, vec![agent_c]);
    }
}

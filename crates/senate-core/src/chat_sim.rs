//! Simulated chat: a message injected into a bound group as if a person
//! had written it on the platform (`senate chat send`), so a chat flow can
//! be a script instead of clicks.
//!
//! The message enters where a platform's messages enter, the gateway, and
//! takes their path from there: the gateway records it as a `chat` event,
//! the Speaker interprets it and posts what it says back through the same
//! surface. Nothing in the Speaker knows it is simulated.
//!
//! - **Marked.** The `chat` event is from the person, as a real message from
//!   them would be, with [`Source::simulated`](crate::Source::simulated)
//!   set and [`Source::simulated_by`](crate::Source::simulated_by) naming
//!   the person who injected it. The Speaker carries a chat's source onto
//!   what it derives from it, so a relay, task or decision made from it is
//!   marked too: "(simulated by bob)".
//! - **Kept off the platform** unless `live`: whatever the Speaker posts for
//!   a simulated message (replies under it, a task card for its relay, and
//!   everything under those) is recorded in the conversation's
//!   [`SimTranscript`] and never reaches the platform. With `live`, the
//!   message itself is first posted into the real group (marked as
//!   simulated) and everything answering it goes to the platform as well as
//!   into the transcript.
//! - **Who may**: a person, as anyone on the roster (themselves or another
//!   person, so a script can have two people agree), or the Speaker's
//!   office, as any person. Never an agent, and never as a guest or an
//!   agent. The server enforces this.
//!
//! Transcripts are held by the daemon in memory: a restart forgets them.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::ids::{CommitteeId, Name, Seq};
use crate::senator::SenatorName;

/// What a simulated message's ref starts with (a `live` one has the
/// platform's ref instead).
pub const SIM_REF_PREFIX: &str = "sim:";

/// `POST`: inject a message ([`SimulateChat`] → [`SimulatedChat`]).
pub fn simulate_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/chat/simulate")
}

/// `GET ?ref=<message_ref>`: a simulated conversation ([`SimTranscript`]).
pub fn simulated_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/chat/simulated")
}

/// The body of a simulate request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulateChat {
    pub text: String,
    /// Who is calling: the person (`None`, or their own name), or one of
    /// their agents, which is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<SenatorName>,
    /// Calling for the Speaker's office, which may send as any person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub office: bool,
    /// Who acts for the office, as `meta.acted_by` names it on a write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acted_by: Option<SenatorName>,
    /// Whom the message is from, any person on the roster; `None`: the
    /// caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_person: Option<Name>,
    /// The bound group; `None`: the committee's only bound group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// A message of a simulated conversation to reply to, in its thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Plain chat that does not mention the bot (default: a mention).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ambient: bool,
    /// Post it, and everything answering it, on the real platform too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live: bool,
}

/// A simulate request once the server has checked who may make it: the
/// message is from `from`, a person on the roster, injected by `by`, the
/// person who called (`from` too, or anyone else).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimRequest {
    pub committee: CommitteeId,
    pub from: Name,
    pub by: Name,
    pub text: String,
    pub group: Option<String>,
    pub reply_to: Option<String>,
    pub ambient: bool,
    pub live: bool,
}

/// The message injected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulatedChat {
    /// The conversation it belongs to: its first message's ref, which
    /// [`simulated_path`] reads.
    pub conversation: String,
    pub message_ref: String,
    pub group_id: String,
    /// Its `chat` event; `None` when the gateway recorded none (nothing to
    /// say, as with a platform message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<Seq>,
    pub live: bool,
}

/// A simulated conversation: the messages injected and everything the
/// Speaker posted for them, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimTranscript {
    pub conversation: String,
    pub committee: CommitteeId,
    pub group_id: String,
    pub live: bool,
    pub entries: Vec<SimEntry>,
}

impl SimTranscript {
    /// The Speaker's posts after the first `since` entries.
    pub fn speaker_posts_after(&self, since: usize) -> impl Iterator<Item = &SimEntry> {
        self.entries
            .iter()
            .skip(since)
            .filter(|e| e.from == Address::Speaker)
    }

    /// How many entries there are up to and including the latest injected
    /// message.
    pub fn through_last_inbound(&self) -> usize {
        self.entries
            .iter()
            .rposition(|e| e.from != Address::Speaker)
            .map_or(0, |i| i + 1)
    }
}

/// One message of a simulated conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimEntry {
    pub ts: DateTime<Utc>,
    /// The person a message was injected as, or [`Address::Speaker`] for
    /// what the Speaker posted.
    pub from: Address,
    /// Who injected a message ([`SimRequest::by`]); `None` for what the
    /// Speaker posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated_by: Option<Name>,
    /// As posted: markdown, or a card's title and body.
    pub text: String,
    pub message_ref: String,
    /// The message it answers, in its thread; `None` for a new root (a
    /// task card in another group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The group it is in, when it is not the conversation's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub card: bool,
    /// An update of the card posted as this ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<String>,
    /// The injected message's `chat` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<Seq>,
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    /// The request cannot be served as asked (answered 400).
    #[error("{0}")]
    Invalid(String),
    /// No such conversation (404).
    #[error("{0}")]
    NotFound(String),
    /// The gateway is not running, or the platform refused (503).
    #[error("{0}")]
    Unavailable(String),
}

/// The gateway side: what the server hands a checked request to.
#[async_trait]
pub trait ChatSimulator: Send + Sync + 'static {
    async fn simulate(&self, request: SimRequest) -> Result<SimulatedChat, SimError>;

    /// The conversation `message_ref` (any message of it) belongs to.
    async fn transcript(
        &self,
        committee: &CommitteeId,
        message_ref: &str,
    ) -> Result<SimTranscript, SimError>;
}

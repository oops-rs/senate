//! Facts about tool approval prompts. These carry no permission decisions
//! and no terminal input bytes.

use senate_core::{Address, CommitteeId, EventId, SenatorName, Seq};
use serde::{Deserialize, Serialize};

/// An observation time shared by the host, hooks and daemon on this machine.
/// A host ID never survives a reboot, so different boots cannot mix times.
pub(crate) fn observed_at() -> u64 {
    use rustix::time::{ClockId, clock_gettime};
    let now = clock_gettime(ClockId::Monotonic);
    (now.tv_sec as u64) * 1_000_000_000 + now.tv_nsec as u64
}

/// One bounded log line. Runtime tool arguments are untrusted text.
pub(crate) fn tool_label(tool: &str) -> String {
    let mut chars = tool.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut label: String = chars.by_ref().take(600).collect();
    if chars.next().is_some() {
        label.push('…');
    }
    label
}

/// The native approval hook's observations for a hosted Claude session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ClaudeApprovalSignal {
    Requested { tool: String, fingerprint: String },
    Pending,
    Finished { fingerprint: String },
}

/// The existing Codex relay's read-only feed to the hosted notice worker.
pub(crate) struct CodexApprovalSignal {
    pub session_id: String,
    pub request_id: String,
    pub at: u64,
    /// The reported command or tool on receipt, and None on native resolution.
    pub tool: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct HostApprovalState {
    pub session_id: String,
    pub claude: Option<ClaudeCandidate>,
    pub notices: Vec<ApprovalNotice>,
}

impl HostApprovalState {
    pub fn is_empty(&self) -> bool {
        self.claude.is_none() && self.notices.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ClaudeCandidate {
    pub tool: String,
    pub fingerprint: String,
    pub at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ApprovalNotice {
    pub id: EventId,
    pub prompt: String,
    pub at: u64,
    pub tool: String,
    pub agent: SenatorName,
    pub committee: CommitteeId,
    /// Fixed before posting so a retry keeps the same recipient and event ID.
    pub recipient: Option<Address>,
    pub seq: Option<Seq>,
    /// The stable ID of its archive status once a handling fact arrives.
    pub archive: Option<EventId>,
}

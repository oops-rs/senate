//! Observations and outcomes of hosted permission prompts.

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
    let mut chars = tool.chars().filter_map(|c| match c {
        '\u{00ad}'
        | '\u{034f}'
        | '\u{061c}'
        | '\u{180b}'..='\u{180f}'
        | '\u{200b}'..='\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}'
        | '\u{fe00}'..='\u{fe0f}'
        | '\u{feff}'
        | '\u{fff9}'..='\u{fffb}'
        | '\u{e0000}'..='\u{e0fff}' => None,
        '\u{2028}' | '\u{2029}' => Some(' '),
        c if c.is_control() => Some(' '),
        c => Some(c),
    });
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
    Requested {
        tool: String,
        fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirmation_label: Option<String>,
    },
    Pending,
    Finished {
        fingerprint: String,
    },
}

/// One permission request currently confirmed by the Codex connection.
#[derive(Clone)]
pub(crate) struct CodexPrompt {
    pub session_id: String,
    pub request_id: String,
    pub at: u64,
    pub tool: String,
}

/// Native request arrivals and resolutions, in observation order.
pub(crate) enum CodexApprovalSignal {
    Opened(CodexPrompt),
    Resolved {
        session_id: String,
        request_id: Option<String>,
        outcome: CodexApprovalOutcome,
        at: u64,
    },
}

pub(crate) enum CodexApprovalOutcome {
    Answered(PromptDecision),
    ResolvedElsewhere,
    Dismissed,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct HostApprovalState {
    pub session_id: String,
    pub claude: Option<ClaudeCandidate>,
    /// Stop fences off hook and screen observations from the ended turn.
    #[serde(default)]
    pub claude_stop_at: u64,
    /// A new request can arrive before a Notification even if terminal
    /// input cleared its candidate in the meantime.
    #[serde(default)]
    pub claude_request_at: u64,
    pub notices: Vec<ApprovalNotice>,
}

impl HostApprovalState {
    pub fn is_empty(&self) -> bool {
        self.claude.is_none()
            && self.claude_stop_at == 0
            && self.claude_request_at == 0
            && self.notices.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ClaudeCandidate {
    pub tool: String,
    pub fingerprint: String,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_label: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PromptDecision {
    pub decision: crate::approvals::Decision,
    pub actor: Option<String>,
}

/// Why a waiting prompt's notice was closed. It is written beside the
/// status that archives the notice ([`senate_core::meta::APPROVAL_CLOSED`]
/// and the event's body), so the log says how each wait ended: without it
/// a prompt a person answered and one that was never shown looked alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CloseCause {
    /// Answered through Senate (`senate session approve`, the app), or by
    /// Codex's app-server on Senate's answer.
    Answered,
    /// Someone typed in its terminal while it waited.
    Typed,
    /// The tool's own result arrived: the prompt was settled in its
    /// terminal.
    ToolRan,
    /// The runtime reported the request resolved outside Senate.
    Resolved,
    /// The turn ended with no answer and no result for the tool: refused
    /// by the runtime's own check, or never shown.
    TurnEnded,
    /// The prompt is no longer on its screen, and nothing answered it.
    PromptGone,
    /// Its terminal ended, or runs another session now.
    SessionEnded,
    /// The terminal moved to another conversation.
    ConversationChanged,
}

impl CloseCause {
    /// The word in the log ([`senate_core::meta::APPROVAL_CLOSED`]).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Typed => "typed",
            Self::ToolRan => "tool_ran",
            Self::Resolved => "resolved",
            Self::TurnEnded => "turn_ended",
            Self::PromptGone => "prompt_gone",
            Self::SessionEnded => "session_ended",
            Self::ConversationChanged => "conversation_changed",
        }
    }

    /// The same, as the closing status says it to a reader.
    pub fn words(self, locale: senate_core::Locale) -> &'static str {
        match self {
            Self::Answered => locale.pick("已通过 Senate 答复。", "Answered through Senate."),
            Self::Typed => {
                locale.pick("有人在它的终端里输入了。", "Someone typed in its terminal.")
            }
            Self::ToolRan => locale.pick(
                "工具的结果已返回：提示已在终端里处理。",
                "The tool's result arrived: the prompt was settled in its terminal.",
            ),
            Self::Resolved => locale.pick(
                "运行时报告该请求已在 Senate 之外处理。",
                "The runtime reported the request resolved outside Senate.",
            ),
            Self::TurnEnded => locale.pick(
                "这一轮已结束，没有答复，工具也没有结果：提示被运行时自己拒绝，或从未显示。",
                "Its turn ended with no answer and no result for the tool: the runtime refused \
                 it itself, or the prompt was never shown.",
            ),
            // Kept as first written: readers and tests know this line.
            Self::PromptGone => locale.pick(
                "终端提示已消失，未收到答复。",
                "The terminal prompt went away unanswered.",
            ),
            Self::SessionEnded => locale.pick("它的终端已结束。", "Its terminal ended."),
            Self::ConversationChanged => locale.pick(
                "终端已切换到另一段对话。",
                "The terminal moved to another conversation.",
            ),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ApprovalNotice {
    pub id: EventId,
    pub prompt: String,
    pub at: u64,
    pub tool: Option<String>,
    pub runtime: crate::session::Runtime,
    pub agent: SenatorName,
    pub committee: CommitteeId,
    /// Fixed before posting so a retry keeps the same recipient and event ID.
    pub recipient: Option<Address>,
    pub seq: Option<Seq>,
    #[serde(default)]
    pub relay_thread: Option<Seq>,
    #[serde(default)]
    pub unseated: bool,
    /// The stable ID of its archive status once a handling fact arrives.
    pub archive: Option<EventId>,
    pub answer: Option<PromptDecision>,
    /// The runtime or host confirmed the wait ended without a recorded answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dismissed: bool,
    /// Why it was closed; none on a notice closed before causes were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<CloseCause>,
    /// The append may have reached the server even if its reply was lost.
    #[serde(default)]
    pub append_attempted: bool,
    #[serde(default)]
    pub failures: u8,
    /// Not a permission prompt the runtime told of: a choice its host saw
    /// waiting on the screen ([`crate::host::prompt`]).
    #[serde(default)]
    pub screen: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_labels_strip_invisible_formatting_and_keep_one_line() {
        assert_eq!(
            tool_label("printf\u{2028}input\u{2029}extra\u{202e}end\u{200d}\u{2066}\u{feff}"),
            "printf input extraend"
        );
    }
}

//! Bounded wakeups from the machine. These are internal inputs. The server
//! resolves each against the authorized view before making a public frame.

use senate_core::{CommitteeId, SenatorName, Seq};
use senate_observe_protocol::{ApprovalDecision, ApprovalTool, HoldReason, RoutePhase};

use crate::CardKey;

#[derive(Clone)]
pub enum LocalNotice {
    Route {
        committee: CommitteeId,
        seq: Seq,
        senator: Option<SenatorName>,
        phase: RoutePhase,
        reason: Option<HoldReason>,
    },
    ApprovalDecided {
        native_id: String,
        committee: CommitteeId,
        senator: SenatorName,
        tool: ApprovalTool,
        decision: ApprovalDecision,
    },
    Preview {
        committee: CommitteeId,
        seq: Seq,
    },
    Tool {
        committee: CommitteeId,
        senator: SenatorName,
        session: String,
        call: String,
        kind: String,
        started: bool,
    },
    Card {
        key: CardKey,
        revision: String,
    },
}

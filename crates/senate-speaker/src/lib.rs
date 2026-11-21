//! The built-in Speaker: it presides over a Senate, backed by a model
//! on basis.
//!
//! The Speaker is the only voice on the chat platform. It reads the log
//! through [`senate_core::Hub`] in `seq` order per committee (named cursor
//! [`CURSOR`], `speaker`) and:
//!
//! - **interprets** a platform `chat` addressed to it with its [`Brain`], or
//!   the fixed syntax (`/bind <committee>`, `/unbind`, `/tell
//!   <name> <text>`, `/status`, `/decide <text>`, `/brief`,
//!   `/agents [<committee>]`, `/invite <handle>…`, `/output
//!   answers|status|all`, `/work`, `/reassign`), which always works and is
//!   the fallback when the Brain fails. `relay` appends a `question`/`note`
//!   to the agents meant and acknowledges in the person's thread;
//!   state-changing actions are restated and confirmed with a yes by the
//!   person who asked (the fixed syntax runs directly); `decide` appends a
//!   `decision` on the person's behalf; `recall` replies with the
//!   committee's brief; `status` reports the binding and who can be
//!   reached.
//! - **routes to agents, never derives them.** A person may have several
//!   agents. "Alice's agent" is a roster lookup: one agent is that agent;
//!   none is Alice herself; several is the agent the asker named
//!   (`codex`, 「alice 的 codex」), else the one [`Brain::pick_agent`]
//!   settles on from what each is doing, else a question in the thread
//!   asking which, answered in the same thread.
//! - **converses** instead, with `mode = "agent"` (see [`SpeakerMode`]): a
//!   `chat` addressed to it that is not fixed syntax goes to
//!   [`Brain::converse`] with the earlier turns of its platform thread; the
//!   model reads (read-only tools) and plans; the engine acts, restating
//!   every state-changing step of the plan as one numbered list confirmed
//!   with one yes.
//! - **triages** every incoming task (a root `question` / `request` to an
//!   agent), parks the ones that need a person on them, and handles their
//!   approval, a person stepping in, and escalation.
//! - **runs the Agent group**: creates or adopts it, invites people, keeps a
//!   card per task thread and two pinned cards updated in place, and
//!   archives it with the last people group.
//! - **mirrors** agents' traffic to the platform. Who wrote what is never
//!   written into a body: a copy is headed by the event's byline
//!   ([`senate_core::Event::byline`]), `[senator::claude]`, or
//!   `[speaker::claude]` when the Speaker answered for that agent,
//!   followed by the localized takeover or companion word when it applies.
//! - **asks people to confirm** an agent's proposed decision.
//! - **writes the brief's top** with [`Brain::brief_top`].
//! - **answers for an agent** that has no live connection when a question
//!   reaches it, or that has not answered within `answer_fallback_secs`.
//! - **takes over** for an agent that is away: its directed questions are
//!   answered from *that agent's own* latest checkpoint, or escalated to
//!   people when the checkpoint is missing, stale or does not cover them,
//!   and handed back when the agent returns. A sibling agent of the same
//!   person never stands in.
//! - **works on request**: a person's `/work <name>
//!   <instruction>`, restated and confirmed, runs a [`senate_work::Runner`]
//!   from that agent's checkpoint on a Speaker branch, and opens the PR with
//!   a [`senate_work::Forge`]; `/reassign <from> <to>` hands one agent's work
//!   to another through [`Hub::reassign`](senate_core::Hub::reassign).
//!
//! Guests (a chat participant not on the roster, a caller from outside) may
//! ask and be answered, but nothing they say changes state.
//!
//! Every text people read is written in the committee's
//! [`Locale`](senate_core::Locale) ([`SpeakerConfig::locale_of`]); commands
//! and confirmation words are understood in Chinese and English always.
//!
//! The roster comes from [`Hub::roster`](senate_core::Hub::roster): who is a
//! person and who an agent, and the person each agent's seat records as
//! running it. Names are flat (`alice`, `claude`); nothing here reads an
//! owner out of a name.
//!
//! It is restart-safe, and no Brain call or post can hold up other
//! committees: see the `engine` module docs.
//!
//! # Wiring
//!
//! ```no_run
//! # async fn wire(
//! #     hub: std::sync::Arc<dyn senate_core::Hub>,
//! #     surface: std::sync::Arc<dyn senate_core::ChatSurface>,
//! #     unbound_rx: tokio::sync::mpsc::Receiver<senate_core::UnboundMention>,
//! #     config: senate_speaker::SpeakerConfig,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use senate_speaker::{BasisBrain, Speaker, SpeakerArgs};
//!
//! let brain = Arc::new(BasisBrain::new(&config, hub.clone()));
//! let (speaker, _join) =
//!     Speaker::spawn(SpeakerArgs::new(hub, surface, brain, config, unbound_rx)).await?;
//! # let _ = speaker; Ok(()) }
//! ```

mod actor;
mod basis_brain;
mod brain;
mod config;
mod engine;
mod ids;
mod render;
mod scripted;
pub mod secretary;
mod syntax;

pub use actor::{RunReport, Speaker, SpeakerArgs, SpeakerMsg, SpeakerState, Thought};
pub use basis_brain::{
    BasisBrain, CheckResult, LOG_READ, LOG_SEARCH, ModelRecipe, ModelReport, PROBE_DEADLINE,
    RuntimeRecipe, TierReport, basis_runtime,
};
pub use brain::{
    AgentCandidate, AgentTurn, Answer, Brain, BrainError, CommitteeSeat, Conversation, Crossing,
    CrossingInput, GroupRef, Intent, Offline, OfflineInput, Pick, PickInput, Question, RelayKind,
    Takeover, TriageInput, Turn, Utterance, apply_threshold, rule_crossing, rule_offline,
    rule_pick, triage_verdict,
};
pub use config::{DEFAULT_BOT_NAME, SpeakerConfig, SpeakerMode, WorkConfig};
pub use engine::{CURSOR, TAKEOVER_META};
pub use ids::operation_id;
pub use scripted::{ScriptedBrain, scripted_verdict};
pub use secretary::{
    BaseLayer, DEFAULT_NAME, Duty, Effective, ModelCheck, ModelSettings, Problem, SecretaryConfigs,
    Shown, ShownValue, Source, Tier,
};

#[cfg(test)]
mod tests;

/// What every text the Speaker writes is held to in this crate's tests.
#[cfg(test)]
pub(crate) mod tests_support {
    /// Panics when `text` uses the predecessor's vocabulary: its address
    /// prefixes, its words for the Speaker and for a committee, or its chat
    /// platform. A git branch such as `feature/refund-v2/alice` is a
    /// developer's convention and is let through.
    pub(crate) fn assert_senate_vocabulary(text: &str) {
        let lower = text.to_lowercase();
        let old_platform = concat!("fei", "shu");
        for word in ["user:", "agent:", "steward", old_platform, "管家", "功能"] {
            assert!(!lower.contains(word), "{word:?} in {text:?}");
        }
        assert!(
            !lower.replace("feature/", "").contains("feature"),
            "\"feature\" in {text:?}"
        );
    }
}

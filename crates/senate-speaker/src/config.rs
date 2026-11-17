//! The `[speaker]` section of the server config.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use senate_core::{BranchNaming, CommitteeId, Locale};
use senate_work::{Budget, DEFAULT_PR_BASE, RepoAllowlist};
use serde::{Deserialize, Serialize};

/// ```toml
/// [speaker]
/// data_dir = "/var/lib/senate"     # the Brain's workspaces live under <data_dir>/speaker/
/// model = "claude-sonnet-4-5"       # optional; else basis's usual env/config resolution
/// answer_fallback_secs = 60         # answer for an agent when a directed question has no answer by then
/// confirmation_ttl_secs = 600       # how long a restatement stays answerable with "yes"
/// brain_deadline_secs = 45          # wall-clock bound on one Brain run (the Speaker gives up 15 s later)
/// brain_token_budget = 80000        # token bound on one Brain run
/// brief_top_debounce_secs = 30      # wait after a decision before rewriting the brief's top
/// triage_deadline_secs = 5          # wall-clock bound on one triage call (then: unsure)
/// card_debounce_ms = 2000           # at most one in-place update per card per this long
/// takeover_stale_hours = 24         # a checkpoint older than this is not answered from (escalated)
/// mode = "intent"                   # "agent": multi-step conversations with tools (agent mode)
/// agent_max_actions = 5             # agent mode: most operations one turn may plan
/// locale = "zh"                     # what people read: "zh" or "en" (default "en")
/// bot_name = "Speaker"              # how people address the bot in chat, e.g. "@Speaker"
///
/// [speaker.locales]                 # a committee whose people read another language
/// refund-v2 = "en"
///
/// [speaker.work]                    # coding work on request; absent = no work runs
/// allowed_repos = ["example/refund"]   # only these repos (owner/name)
/// max_concurrent_runs = 1           # runs at once on this server (and always one per committee)
/// pr_base = "main"                  # what the PR merges into
/// branch_prefix = "feature"         # branches are <prefix>/<committee>/speaker-<n>
/// budget = { wall_secs = 1800, max_turns = 200 }
/// [speaker.work.developers]         # who may ask, besides people whose agent is active on the committee
/// refund-v2 = ["alice", "bob"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerConfig {
    /// Where the Brain keeps its workspaces. The daemon fills it in from
    /// its own `data_dir`; a `secretary.toml` never needs to.
    #[serde(default)]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_answer_fallback_secs")]
    pub answer_fallback_secs: u64,
    #[serde(default = "default_confirmation_ttl_secs")]
    pub confirmation_ttl_secs: u64,
    #[serde(default = "default_brain_deadline_secs")]
    pub brain_deadline_secs: u64,
    #[serde(default = "default_brain_token_budget")]
    pub brain_token_budget: u64,
    #[serde(default = "default_brief_top_debounce_secs")]
    pub brief_top_debounce_secs: u64,
    #[serde(default = "default_triage_deadline_secs")]
    pub triage_deadline_secs: u64,
    #[serde(default = "default_card_debounce_ms")]
    pub card_debounce_ms: u64,
    #[serde(default = "default_takeover_stale_hours")]
    pub takeover_stale_hours: u64,
    /// How the Speaker understands what people say to it.
    #[serde(default)]
    pub mode: SpeakerMode,
    /// Agent mode: the most operations one turn may plan.
    #[serde(default = "default_agent_max_actions")]
    pub agent_max_actions: usize,
    /// Coding work on request; `None`: the Speaker does no coding work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkConfig>,
    /// The language every text the Speaker shows people is written in,
    /// unless [`SpeakerConfig::locales`] names another for a committee. A
    /// committee has one language because a group message is read by
    /// everyone in it. Prompts to the model are English and ask for replies
    /// in this language.
    #[serde(default)]
    pub locale: Locale,
    /// Committees whose people read another language than
    /// [`SpeakerConfig::locale`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locales: BTreeMap<CommitteeId, Locale>,
    /// How people address the bot in chat (`@<bot_name>`). The chat
    /// platform names the bot, not Senate, so the Speaker only ever quotes
    /// this when it tells people how to reach it.
    #[serde(default = "default_bot_name")]
    pub bot_name: String,
}

/// The bot's display name when the configuration does not give one.
pub const DEFAULT_BOT_NAME: &str = "Speaker";

fn default_bot_name() -> String {
    DEFAULT_BOT_NAME.to_owned()
}

/// `[speaker] mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeakerMode {
    /// One message, one [`Intent`](crate::Intent) ([`Brain::interpret`](crate::Brain::interpret)).
    #[default]
    Intent,
    /// A conversation per thread with tools and a plan of several operations
    /// ([`Brain::converse`](crate::Brain::converse)).
    Agent,
}

fn default_agent_max_actions() -> usize {
    5
}

/// `[speaker.work]`: coding work on request. The runner and the forge
/// themselves are the daemon's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkConfig {
    /// Only these repositories (`owner/name`); a checkpoint elsewhere is refused.
    #[serde(default, alias = "repos")]
    pub allowed_repos: RepoAllowlist,
    #[serde(default)]
    pub budget: Budget,
    /// Per committee, the people (by handle) that may ask for work besides
    /// those with an agent active on the committee.
    #[serde(default)]
    pub developers: BTreeMap<String, Vec<String>>,
    /// Runs at once on this server. A committee never has more than one.
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: usize,
    /// What a run's PR merges into.
    #[serde(default = "default_pr_base")]
    pub pr_base: String,
    /// How branches are named: a run pushes `<prefix>/<committee>/speaker-<n>`.
    /// A git convention, `feature` unless a deployment says otherwise.
    #[serde(default, rename = "branch_prefix")]
    pub branches: BranchNaming,
}

fn default_max_concurrent_runs() -> usize {
    1
}

fn default_pr_base() -> String {
    DEFAULT_PR_BASE.to_owned()
}

impl WorkConfig {
    pub fn new(allowed_repos: RepoAllowlist) -> Self {
        Self {
            allowed_repos,
            budget: Budget::default(),
            developers: BTreeMap::new(),
            max_concurrent_runs: default_max_concurrent_runs(),
            pr_base: default_pr_base(),
            branches: BranchNaming::default(),
        }
    }

    /// Whether `handle` is listed as a developer of `committee`.
    pub fn lists_developer(&self, committee: &str, handle: &str) -> bool {
        self.developers
            .get(committee)
            .is_some_and(|list| list.iter().any(|h| h.eq_ignore_ascii_case(handle)))
    }
}

fn default_takeover_stale_hours() -> u64 {
    24
}

fn default_triage_deadline_secs() -> u64 {
    5
}

fn default_card_debounce_ms() -> u64 {
    2_000
}

fn default_answer_fallback_secs() -> u64 {
    60
}

fn default_confirmation_ttl_secs() -> u64 {
    600
}

fn default_brain_deadline_secs() -> u64 {
    45
}

fn default_brain_token_budget() -> u64 {
    80_000
}

fn default_brief_top_debounce_secs() -> u64 {
    30
}

impl SpeakerConfig {
    /// A config rooted at `data_dir` with every other field at its default.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            model: None,
            answer_fallback_secs: default_answer_fallback_secs(),
            confirmation_ttl_secs: default_confirmation_ttl_secs(),
            brain_deadline_secs: default_brain_deadline_secs(),
            brain_token_budget: default_brain_token_budget(),
            brief_top_debounce_secs: default_brief_top_debounce_secs(),
            triage_deadline_secs: default_triage_deadline_secs(),
            card_debounce_ms: default_card_debounce_ms(),
            takeover_stale_hours: default_takeover_stale_hours(),
            mode: SpeakerMode::default(),
            agent_max_actions: default_agent_max_actions(),
            work: None,
            locale: Locale::default(),
            locales: BTreeMap::new(),
            bot_name: default_bot_name(),
        }
    }

    /// The language `committee`'s people read.
    pub fn locale_of(&self, committee: &CommitteeId) -> Locale {
        self.locales.get(committee).copied().unwrap_or(self.locale)
    }

    /// The language of `committee`'s people; the configured one for the
    /// groups bound to none.
    pub fn locale_of_opt(&self, committee: Option<&CommitteeId>) -> Locale {
        committee.map_or(self.locale, |c| self.locale_of(c))
    }

    /// `<data_dir>/speaker`, where the Brain keeps its per-committee
    /// workspaces and its history.
    pub fn speaker_dir(&self) -> PathBuf {
        self.data_dir.join("speaker")
    }

    pub fn answer_fallback(&self) -> Duration {
        Duration::from_secs(self.answer_fallback_secs)
    }

    pub fn confirmation_ttl(&self) -> Duration {
        Duration::from_secs(self.confirmation_ttl_secs)
    }

    pub fn brain_deadline(&self) -> Duration {
        Duration::from_secs(self.brain_deadline_secs.max(1))
    }

    pub fn brief_top_debounce(&self) -> Duration {
        Duration::from_secs(self.brief_top_debounce_secs)
    }

    pub fn triage_deadline(&self) -> Duration {
        Duration::from_secs(self.triage_deadline_secs.max(1))
    }

    pub fn card_debounce(&self) -> Duration {
        Duration::from_millis(self.card_debounce_ms)
    }

    /// How old a checkpoint may be for the Speaker to answer from it.
    pub fn takeover_stale(&self) -> Duration {
        Duration::from_secs(self.takeover_stale_hours.saturating_mul(3_600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_section_takes_the_defaults() {
        let config: SpeakerConfig =
            serde_json::from_value(serde_json::json!({ "data_dir": "/var/lib/senate" }))
                .expect("parses");
        assert_eq!(config, SpeakerConfig::new("/var/lib/senate"));
        assert_eq!(config.answer_fallback(), Duration::from_secs(60));
        assert_eq!(config.confirmation_ttl(), Duration::from_secs(600));
        assert_eq!(config.brief_top_debounce(), Duration::from_secs(30));
        assert_eq!(config.triage_deadline(), Duration::from_secs(5));
        assert_eq!(config.card_debounce(), Duration::from_millis(2_000));
        assert_eq!(config.takeover_stale(), Duration::from_secs(24 * 3_600));
        assert_eq!(
            config.speaker_dir(),
            PathBuf::from("/var/lib/senate/speaker")
        );
        assert_eq!(config.work, None);
        assert_eq!(config.mode, SpeakerMode::Intent);
        assert_eq!(config.agent_max_actions, 5);
        assert_eq!(config.locale, Locale::En);
        assert_eq!(config.bot_name, "Speaker");
    }

    #[test]
    fn a_committee_may_read_another_language() {
        let config: SpeakerConfig = serde_json::from_value(serde_json::json!({
            "data_dir": "/d",
            "locale": "zh-CN",
            "locales": { "refund-v2": "en" },
            "bot_name": "Helper"
        }))
        .expect("parses");
        let refund = CommitteeId::new("refund-v2").expect("id");
        let other = CommitteeId::new("other").expect("id");
        assert_eq!(config.locale_of(&refund), Locale::En);
        assert_eq!(config.locale_of(&other), Locale::Zh);
        assert_eq!(config.bot_name, "Helper");
        assert!(
            serde_json::from_value::<SpeakerConfig>(
                serde_json::json!({ "data_dir": "/d", "locale": "fr" })
            )
            .is_err()
        );
    }

    #[test]
    fn agent_mode_parses() {
        let config: SpeakerConfig =
            serde_json::from_value(serde_json::json!({ "data_dir": "/d", "mode": "agent" }))
                .expect("parses");
        assert_eq!(config.mode, SpeakerMode::Agent);
    }

    #[test]
    fn a_work_section_parses() {
        let config: SpeakerConfig = serde_json::from_value(serde_json::json!({
            "data_dir": "/d",
            "work": { "repos": ["o/r"], "developers": { "refund-v2": ["Bob"] }, "branch_prefix": "team" }
        }))
        .expect("parses");
        let work = config.work.expect("work");
        assert!(work.allowed_repos.allows("O/R"));
        assert_eq!(work.max_concurrent_runs, 1);
        assert_eq!(work.pr_base, "main");
        assert!(work.lists_developer("refund-v2", "bob"));
        assert!(!work.lists_developer("other", "bob"));
        assert_eq!(work.branches, BranchNaming::new("team"));
    }
}

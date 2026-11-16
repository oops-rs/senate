//! Work runs: the Speaker doing coding work on a person's request, and how a
//! run shows up in the log.
//!
//! # A run is a task thread
//!
//! A run's thread is rooted at a Speaker `note` with no `to` (nobody's
//! session is pushed it), `on_behalf_of` the person who asked, and
//! [`meta::WORK`] carrying a [`WorkRun`] in state
//! [`WorkState::Queued`]. [`TaskBoard`](crate::TaskBoard) treats such a root
//! as a task, so the run gets a card and the server's `threads` route like
//! any other task. Every later change is a Speaker `status` in the thread
//! whose `meta.work` is the run as it is now (the whole [`WorkRun`], not a
//! diff) and whose [`StatusPayload`](crate::StatusPayload) moves the card:
//! steps while it runs, the PR / branch links and the summary when it ends.
//!
//! | [`WorkState`] | card ([`StatusState`]) |
//! | --- | --- |
//! | `queued`, `running` | `processing` |
//! | `succeeded` | `active` |
//! | `failed`, `timed_out` | `suspended`, waiting on the requester |
//! | `refused` | `archived` |
//!
//! Only the Speaker writes `meta.work`; the server refuses the key from
//! senators. The latest `meta.work` in a thread is the run's state
//! ([`WorkRun::latest`]), so a restarted Speaker knows from the log alone
//! which run was in flight.
//!
//! # Whose work
//!
//! A run starts from one agent's checkpoint, so it names that agent by path
//! ([`WorkRun::target_agent`]). A person asking for "alice's work" to be
//! continued has not said which of alice's agents; that is settled against
//! the roster before a run exists, and a run never holds a person where an
//! agent belongs. The one who asked is always a person
//! ([`WorkRun::requester`]): guests and agents do not order work.
//!
//! # Branches
//!
//! Branch names follow a developer habit that is not Senate's to rename:
//! `feature/<committee>/<person>` for a developer's own branch, and
//! `feature/<committee>/speaker-<run>` for the branch a run pushes.
//! [`BranchNaming`] holds the prefix, [`DEFAULT_BRANCH_PREFIX`] by default,
//! and is the only place these names are put together.
//!
//! # Reassignment
//!
//! Handing an agent's work to another agent is presence, not a work run: a
//! server-written `presence` event with
//! [`PresenceState::Reassigned`](crate::PresenceState::Reassigned) and
//! [`new_owner`](crate::PresencePayload::new_owner), requested by the Speaker
//! through [`Hub::reassign`](crate::Hub::reassign). It stands until it is
//! explicitly undone (a reassignment of the agent back to itself, which
//! writes `online`); the old owner's heartbeats do not end it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::event::{Event, Kind, StatusState, meta};
use crate::ids::{CommitteeId, Name, Seq};
use crate::locale::Locale;
use crate::senator::SenatorPath;

/// The first segment of a branch name unless a deployment says otherwise.
/// A git convention, not Senate's word for a committee.
pub const DEFAULT_BRANCH_PREFIX: &str = "feature";

/// What a run's branch is called after the committee: `speaker-<run>`.
const SPEAKER_BRANCH_STEM: &str = "speaker-";

/// How branches are named: `<prefix>/<committee>/<who>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BranchNaming {
    prefix: String,
}

impl Default for BranchNaming {
    fn default() -> Self {
        Self::new(DEFAULT_BRANCH_PREFIX)
    }
}

impl BranchNaming {
    /// `prefix` may itself hold slashes (`team/feature`); slashes at its
    /// ends are dropped. An empty prefix names branches `<committee>/<who>`.
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.trim_matches('/').to_string(),
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn committee_dir(&self, committee: &CommitteeId) -> String {
        if self.prefix.is_empty() {
            format!("{committee}/")
        } else {
            format!("{}/{committee}/", self.prefix)
        }
    }

    /// A developer's own branch on a committee:
    /// `feature/<committee>/<person>`. It is the person's, whichever of
    /// their agents works on it.
    pub fn developer(&self, committee: &CommitteeId, person: &Name) -> String {
        format!("{}{person}", self.committee_dir(committee))
    }

    /// The branch a work run creates and pushes:
    /// `feature/<committee>/speaker-<run_id>`.
    pub fn speaker(&self, committee: &CommitteeId, run_id: u64) -> String {
        format!(
            "{}{SPEAKER_BRANCH_STEM}{run_id}",
            self.committee_dir(committee)
        )
    }

    /// Is `branch` a work run's branch on `committee`?
    pub fn is_speaker(&self, committee: &CommitteeId, branch: &str) -> bool {
        branch
            .strip_prefix(&self.committee_dir(committee))
            .and_then(|rest| rest.strip_prefix(SPEAKER_BRANCH_STEM))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }
}

/// Where a run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    /// Confirmed; waiting for a runner.
    Queued,
    Running,
    Succeeded,
    /// The agent or the runner failed; any work is kept on the branch.
    Failed,
    /// The wall clock ran out; any work is kept on the branch.
    TimedOut,
    /// A rule refused the order; nothing ran.
    Refused,
}

impl WorkState {
    pub const ALL: [WorkState; 6] = [
        Self::Queued,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::TimedOut,
        Self::Refused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Refused => "refused",
        }
    }

    /// The run is over: nothing more will happen to it on its own.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }

    /// The card state this run state shows as (module docs).
    pub fn status_state(self) -> StatusState {
        match self {
            Self::Queued | Self::Running => StatusState::Processing,
            Self::Succeeded => StatusState::Active,
            Self::Failed | Self::TimedOut => StatusState::Suspended,
            Self::Refused => StatusState::Archived,
        }
    }

    /// The state in words, as the card and the thread show it.
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            Self::Queued => locale.pick("排队中", "queued"),
            Self::Running => locale.pick("运行中", "running"),
            Self::Succeeded => locale.pick("已完成", "done"),
            Self::Failed => locale.pick("失败", "failed"),
            Self::TimedOut => locale.pick("超时", "timed out"),
            Self::Refused => locale.pick("未运行", "not run"),
        }
    }
}

impl std::fmt::Display for WorkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `meta["work"]`: one run, as it is at this event. Written by the Speaker
/// only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRun {
    /// The Speaker's run number: the seq of the `chat` that asked. The
    /// branch is [`BranchNaming::speaker`] of it.
    pub run_id: u64,
    /// The person who asked and confirmed.
    pub requester: Name,
    /// The agent whose checkpoint the run starts from.
    pub target_agent: SenatorPath,
    /// The task, in the requester's words.
    pub instruction: String,
    /// The branch the run creates and pushes.
    pub branch: String,
    pub state: WorkState,
    /// The checkpoint branch the run starts from (read, never written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// The checkpoint commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The checkpoint's remote URL (no credentials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    /// The seq of the checkpoint event the run starts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_seq: Option<Seq>,
    /// The `chat` whose thread the request was restated and confirmed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_in: Option<Seq>,
    /// The PR, once opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// The branch head pushed, if anything was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed_commit: Option<String>,
    /// What was done, or why it stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What a person could do next (after a run that did not succeed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
}

impl WorkRun {
    pub const META_KEY: &'static str = meta::WORK;

    /// The run carried by `event`'s `meta.work`, if the Speaker wrote it and
    /// it parses.
    pub fn of(event: &Event) -> Option<Self> {
        if event.from != Address::Speaker {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::WORK)?.clone()).ok()
    }

    /// The payload as a `meta` value.
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The latest run state in a thread's events (root first, as
    /// [`Hub::thread`](crate::Hub::thread) returns them).
    pub fn latest<'a>(thread: impl IntoIterator<Item = &'a Event>) -> Option<Self> {
        thread.into_iter().filter_map(Self::of).last()
    }
}

/// Whether `event` is the root of a work run's thread (module docs).
pub fn is_work_root(event: &Event) -> bool {
    event.kind == Kind::Note
        && event.thread.is_none()
        && event.from == Address::Speaker
        && event.meta.contains_key(meta::WORK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NewEvent;
    use crate::testing::assert_senate_vocabulary;
    use chrono::Utc;

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn run(state: WorkState) -> WorkRun {
        let branches = BranchNaming::default();
        WorkRun {
            run_id: 7,
            requester: Name::new("bob").unwrap(),
            target_agent: "alice/claude".parse().unwrap(),
            instruction: "把重试路径做完".into(),
            branch: branches.speaker(&c(), 7),
            state,
            base_branch: Some(branches.developer(&c(), &Name::new("alice").unwrap())),
            base_commit: Some("abc".into()),
            repo_url: None,
            checkpoint_seq: Some(Seq(3)),
            requested_in: Some(Seq(7)),
            pr_url: None,
            pushed_commit: None,
            summary: None,
            next_steps: Vec::new(),
        }
    }

    #[test]
    fn a_run_round_trips_and_only_the_speakers_counts() {
        let root = NewEvent::new(c(), Kind::Note, Address::Speaker, "把重试路径做完")
            .with_meta(meta::WORK, run(WorkState::Queued).value())
            .into_event(Seq(8), Utc::now());
        assert!(is_work_root(&root));
        assert_eq!(WorkRun::of(&root), Some(run(WorkState::Queued)));
        assert_eq!(root.meta[meta::WORK]["state"], "queued");
        let forged = NewEvent::new(c(), Kind::Note, "senator::eve/claude".parse().unwrap(), "x")
            .with_meta(meta::WORK, run(WorkState::Succeeded).value())
            .into_event(Seq(9), Utc::now());
        assert!(!is_work_root(&forged));
        assert_eq!(WorkRun::of(&forged), None);
        let done = NewEvent::new(c(), Kind::Status, Address::Speaker, "")
            .in_thread(Seq(8))
            .with_meta(meta::WORK, run(WorkState::TimedOut).value())
            .into_event(Seq(10), Utc::now());
        let latest = WorkRun::latest([&root, &forged, &done]).unwrap();
        assert_eq!(latest.state, WorkState::TimedOut);
        assert!(latest.state.is_terminal());
        assert_eq!(latest.state.status_state(), StatusState::Suspended);
        assert!(!WorkState::Running.is_terminal());
    }

    #[test]
    fn a_run_names_a_person_who_asked_and_one_agent_to_start_from() {
        let value = run(WorkState::Queued).value();
        assert_eq!(value["requester"], "bob");
        assert_eq!(value["target_agent"], "alice/claude");
        // Neither field takes the other's shape, or a wildcard.
        for (field, bad) in [
            ("requester", "bob/claude"),
            ("requester", "senator::bob"),
            ("target_agent", "alice/*"),
            ("target_agent", "senator::alice/claude"),
        ] {
            let mut v = value.clone();
            v[field] = bad.into();
            assert!(
                serde_json::from_value::<WorkRun>(v).is_err(),
                "{field}={bad}"
            );
        }
    }

    #[test]
    fn branches_keep_the_developer_convention_behind_one_prefix() {
        let branches = BranchNaming::default();
        assert_eq!(branches.prefix(), DEFAULT_BRANCH_PREFIX);
        let alice = Name::new("alice").unwrap();
        assert_eq!(branches.developer(&c(), &alice), "feature/refund-v2/alice");
        assert_eq!(branches.speaker(&c(), 7), "feature/refund-v2/speaker-7");

        assert!(branches.is_speaker(&c(), "feature/refund-v2/speaker-12"));
        for not in [
            "feature/refund-v2/alice",
            "feature/refund-v2/speaker-",
            "feature/refund-v2/speaker-1x",
            "feature/other/speaker-1",
            "feature/refund-v2/speaker-1/alice",
            "feature/refund-v2/steward-1",
            "refund-v2/speaker-1",
        ] {
            assert!(!branches.is_speaker(&c(), not), "{not}");
        }
    }

    #[test]
    fn the_branch_prefix_is_a_parameter() {
        let team = BranchNaming::new("/team/topic/");
        assert_eq!(team.prefix(), "team/topic");
        assert_eq!(team.speaker(&c(), 3), "team/topic/refund-v2/speaker-3");
        assert!(team.is_speaker(&c(), "team/topic/refund-v2/speaker-3"));
        assert!(!team.is_speaker(&c(), "feature/refund-v2/speaker-3"));

        let bare = BranchNaming::new("");
        assert_eq!(bare.speaker(&c(), 3), "refund-v2/speaker-3");
        assert!(bare.is_speaker(&c(), "refund-v2/speaker-3"));

        let from_config: BranchNaming = serde_json::from_value("wip".into()).unwrap();
        assert_eq!(from_config, BranchNaming::new("wip"));
    }

    #[test]
    fn a_run_state_is_labelled_in_each_language() {
        let zh: Vec<_> = WorkState::ALL.iter().map(|s| s.label(Locale::Zh)).collect();
        assert_eq!(zh, ["排队中", "运行中", "已完成", "失败", "超时", "未运行"]);
        let en: Vec<_> = WorkState::ALL.iter().map(|s| s.label(Locale::En)).collect();
        assert_eq!(
            en,
            [
                "queued",
                "running",
                "done",
                "failed",
                "timed out",
                "not run"
            ]
        );
        for label in zh.iter().chain(&en) {
            assert_senate_vocabulary(label);
        }
        // The wire name is not a label and is never translated.
        assert_eq!(WorkState::TimedOut.to_string(), "timed_out");
    }
}

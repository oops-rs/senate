//! The Speaker's hands: coding work done on a person's request, on a branch
//! of the Speaker's own, delivered as that branch or as a pull request.
//!
//! - [`WorkOrder`]: what a person asked for, whose checkpoint it starts from,
//!   and on which branch it lands. [`WorkOrder::validate`] holds the rules:
//!   only listed repositories, only the committee's Speaker branches
//!   (`<prefix>/<committee>/speaker-<n>`, named by
//!   [`BranchNaming`](senate_core::BranchNaming)), never the checkpoint
//!   branch a developer works on.
//! - Whose work: the one who asked is always a person
//!   ([`WorkOrder::requester`]), and the run starts from one agent's
//!   checkpoint ([`WorkOrder::target_agent`]). A person may have several
//!   agents; which one is meant is settled before an order exists.
//! - [`Runner`]: does the order and reports [`RunProgress`] steps for the
//!   task card. [`DockerRunner`] runs Claude Code headless in a container
//!   over a throwaway checkout; work is committed and pushed even when the
//!   run fails or times out, so nothing done is lost before a person is
//!   asked what next.
//! - [`Forge`]: opens the PR. [`GhForge`] uses `gh`; [`NoForge`] delivers the
//!   branch alone.
//! - [`testing`] (feature `testing`): [`testing::FakeRunner`] and
//!   [`testing::FakeForge`].
//!
//! Text this crate writes for people (step labels, summaries, next steps,
//! the PR) is in the order's [`Locale`](senate_core::Locale). Commit
//! messages are English: they are git history, read by tools as much as by
//! people.

mod docker;
mod forge;
mod git;
mod order;
mod proc;
pub mod prompt;
mod runner;
pub mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use docker::{
    DockerRunner, DockerRunnerConfig, default_allowed_tools, default_disallowed_tools,
};
pub use forge::{
    Delivery, Forge, ForgeError, GhForge, GhForgeConfig, NoForge, PrRef, PrRequest,
    pr_number_from_url,
};
pub use git::{GitCredential, SPEAKER_EMAIL, SPEAKER_NAME, commit_message, container_name};
pub use order::{
    Budget, DEFAULT_MAX_TURNS, DEFAULT_PR_BASE, DEFAULT_WALL_SECS, RepoAllowlist, RepoRef,
    WorkError, WorkOrder, check_push_target, checkpoint_notes, sanitize_ref_component,
    slug_from_url, valid_branch_name, valid_commit, valid_slug,
};
pub use prompt::build_prompt;
pub use runner::{RunOutcome, RunProgress, RunStatus, Runner, clip_label, default_next_steps};
pub use senate_core::CheckpointFiles;

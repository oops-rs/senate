//! `senate join`, `senate leave`, `senate pin`, `senate unpin`, `senate
//! where`, `senate list` and `senate free`: binding the worktree you are in
//! to a committee, and seeing and cleaning what Senate keeps. None needs the
//! daemon; a live session in the worktree reads a change at its next prompt.
//!
//! `join` and `pin` write `.senate/committee` and `.senate/agent` in the
//! current worktree, after making `.senate/` match the checked-out branch
//! ([`crate::follow`]), so they bind *this worktree on this branch*: switch
//! to another branch and that branch's own copy comes back. Nothing is
//! written under `.git`, and `.senate/` is never tracked.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use senate_core::{BranchNaming, CommitteeId, Locale, Name};
use serde::{Deserialize, Serialize};

use crate::agent::{self, AgentSource, Pins, RelPath, Seating};
use crate::config::AgentNames;
use crate::follow::{self, Slot, SlotKey, Stale};
use crate::joinname::{self, BranchPart, Cannot, Choice, Derived, NameFrom};
use crate::paths::Paths;
use crate::session::{self, CommitteeSource, GitFacts, Runtime};
use crate::state::Store;

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Sessions that ended (or went stale) are forgotten after this long.
pub const ENDED_SESSION_DAYS: i64 = 7;
/// A session whose process is gone is forgotten after this long.
pub const DEAD_SESSION_DAYS: i64 = 1;
/// Default of `free_after_days`.
pub const DEFAULT_FREE_AFTER_DAYS: u64 = 30;

/// What the commands need of `senate.toml`: read leniently, so a missing or
/// half-written file leaves the defaults and nothing here fails on it.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub locale: Locale,
    #[serde(default)]
    pub person: Option<Name>,
    #[serde(default)]
    committee_from_branch: bool,
    #[serde(default)]
    branch_prefix: Option<String>,
    #[serde(default)]
    agents: AgentNames,
    #[serde(default)]
    committees: Vec<CommitteeId>,
    /// How long an agent with no session and no contact keeps its queued
    /// items and cursors.
    #[serde(default = "default_free_after")]
    pub free_after_days: u64,
    #[serde(default)]
    local: Option<toml::Table>,
    #[serde(default)]
    preside: Vec<Presides>,
}

#[derive(Debug, Clone, Deserialize)]
struct Presides {
    agent: RelPath,
}

fn default_free_after() -> u64 {
    DEFAULT_FREE_AFTER_DAYS
}

impl Default for Settings {
    fn default() -> Self {
        toml::from_str("").unwrap_or_else(|_| unreachable!("every key has a default"))
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// The branch naming a committee is read from, when switched on.
    pub fn convention(&self) -> Option<BranchNaming> {
        self.committee_from_branch.then(|| {
            self.branch_prefix
                .as_deref()
                .map_or_else(BranchNaming::default, BranchNaming::new)
        })
    }

    /// The directories under `senates/` that `senate.toml` still hosts.
    fn hosted_keys(&self) -> BTreeSet<String> {
        let mut keys = BTreeSet::new();
        if self.local.is_some() {
            keys.insert("top".to_string());
        }
        for p in &self.preside {
            keys.insert(format!("under-{}", p.agent.to_string().replace('/', "-")));
        }
        keys
    }
}

/// The agent a session of one harness would speak as in a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHere {
    pub runtime: Runtime,
    pub rel: RelPath,
    /// In full, when `senate.toml` names the person.
    pub path: Option<senate_core::SenatorPath>,
    pub source: AgentSource,
}

/// What a directory resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub cwd: PathBuf,
    pub facts: GitFacts,
    pub committee: Option<(CommitteeId, CommitteeSource)>,
    pub agents: Vec<AgentHere>,
    /// The slot this worktree's `.senate/` belongs to.
    pub belongs_to: Option<SlotKey>,
    /// Saved copies for other branches of this repository.
    pub others: Vec<Slot>,
}

impl Resolved {
    /// The decision, from what was found. `agent_env` is `SENATE_AGENT`.
    pub fn from_facts(
        cwd: &Path,
        facts: GitFacts,
        settings: &Settings,
        agent_env: Option<&str>,
    ) -> Self {
        let convention = settings.convention();
        let committee = session::infer_committee(
            facts.branch.as_deref(),
            facts.committee_file.as_deref(),
            convention.as_ref(),
        );
        let seating = settings.person.clone().map(Seating::top);
        let agents = [Runtime::Claude, Runtime::Codex]
            .into_iter()
            .map(|runtime| {
                let (rel, source) = agent::choose(
                    &settings.agents,
                    runtime,
                    Pins {
                        env: agent_env,
                        file: facts.agent_file.as_deref(),
                    },
                );
                AgentHere {
                    runtime,
                    path: seating.as_ref().map(|s| s.place(&rel)),
                    rel,
                    source,
                }
            })
            .collect();
        Self {
            cwd: cwd.to_path_buf(),
            facts,
            committee,
            agents,
            belongs_to: None,
            others: Vec::new(),
        }
    }
}

/// Resolve `cwd` as a session there would be bound: `.senate/` made to
/// match the branch first.
pub async fn resolve(paths: &Paths, cwd: &Path, settings: &Settings) -> Resolved {
    let (facts, _) = session::gather_synced(paths, cwd, GIT_TIMEOUT).await;
    let env = std::env::var(agent::AGENT_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let tree = facts.tree();
    let mut resolved = Resolved::from_facts(cwd, facts, settings, env.as_deref());
    if let Some(tree) = tree {
        resolved.belongs_to = follow::belongs_to(&tree);
        resolved.others = follow::slots(paths)
            .into_iter()
            .filter(|s| s.meta.repository == tree.common_dir && s.meta.key != tree.key())
            .collect();
    }
    resolved
}

/// The committee named, else the one this directory resolves to.
pub async fn committee_here(
    paths: &Paths,
    named: Option<CommitteeId>,
    settings: &Settings,
) -> anyhow::Result<CommitteeId> {
    if let Some(committee) = named {
        return Ok(committee);
    }
    let cwd = std::env::current_dir()?;
    resolve(paths, &cwd, settings)
        .await
        .committee
        .map(|(c, _)| c)
        .context("name the committee with --committee (nothing here binds one: see `senate join`)")
}

// ---- join, leave, pin, unpin ------------------------------------------------

/// Which of `.senate/`'s files a command writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Committee,
    Agent,
}

impl Item {
    fn file(self) -> &'static str {
        match self {
            Self::Committee => "committee",
            Self::Agent => "agent",
        }
    }
}

/// What a `join`, `leave`, `pin` or `unpin` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub item: Item,
    /// The value written; `None` for a removal.
    pub value: Option<String>,
    /// For a removal: whether anything was there.
    pub removed: bool,
    /// The branch the file now belongs to (`None`: detached, or no git).
    pub branch: Option<String>,
    pub in_git: bool,
    pub file: PathBuf,
    pub after: Resolved,
}

/// Write (`Some`) or remove (`None`) one file of `.senate/` for the
/// worktree of `cwd`, with `.senate/` in step with its branch.
pub async fn change(
    paths: &Paths,
    cwd: &Path,
    item: Item,
    value: Option<String>,
    settings: &Settings,
) -> anyhow::Result<Change> {
    let paths_owned = paths.clone();
    let cwd_owned = cwd.to_path_buf();
    let file_name = item.file();
    let value_owned = value.clone();
    let (file, removed, branch, in_git) = tokio::task::spawn_blocking(move || {
        let tree = follow::tree_of(&cwd_owned);
        let (base, _guard) = match &tree {
            Some(tree) => {
                let (lock, senate) = follow::prepare(&paths_owned, tree)?;
                (senate, Some(lock))
            }
            None => (session::ensure_senate_dir(&cwd_owned)?, None),
        };
        // A removal where nothing was ever written creates nothing: undo
        // the directory `prepare` made only when it holds nothing of ours.
        let file = base.join(file_name);
        let removed = match &value_owned {
            Some(text) => {
                let mut text = text.clone();
                text.push('\n');
                let tmp = base.join(format!(".{file_name}.tmp-{}", std::process::id()));
                std::fs::write(&tmp, text)?;
                std::fs::rename(&tmp, &file)?;
                false
            }
            None => match std::fs::remove_file(&file) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(anyhow::Error::from(e)),
            },
        };
        Ok::<_, anyhow::Error>((
            file,
            removed,
            tree.as_ref().and_then(|t| t.branch.clone()),
            tree.is_some(),
        ))
    })
    .await??;
    let after = resolve(paths, cwd, settings).await;
    Ok(Change {
        item,
        value,
        removed,
        branch,
        in_git,
        file,
        after,
    })
}

/// Validate and bind the worktree to `committee`.
pub async fn join(
    paths: &Paths,
    cwd: &Path,
    committee: &CommitteeId,
    settings: &Settings,
) -> anyhow::Result<Change> {
    change(
        paths,
        cwd,
        Item::Committee,
        Some(committee.to_string()),
        settings,
    )
    .await
}

/// [`join`] with the name optional: without one, the name
/// [`joinname::derive`] makes from where this is run, read-only. Fails, with
/// nothing written, when none can be made.
pub async fn join_choosing(
    paths: &Paths,
    cwd: &Path,
    explicit: Option<CommitteeId>,
    settings: &Settings,
) -> anyhow::Result<(Change, Choice)> {
    let facts = match explicit {
        Some(_) => joinname::Facts::default(),
        None => joinname::gather(cwd, GIT_TIMEOUT).await,
    };
    let choice = joinname::choose(explicit, &facts)
        .map_err(|cannot| anyhow::anyhow!("{}", cannot_join_text(&cannot, settings.locale)))?;
    let change = join(paths, cwd, choice.id(), settings).await?;
    Ok((change, choice))
}

/// What `senate join` with no name would do here: for `senate where`.
pub async fn would_join(cwd: &Path) -> Result<Derived, Cannot> {
    joinname::derive(&joinname::gather(cwd, GIT_TIMEOUT).await)
}

fn why_cannot(c: &Cannot, l: Locale) -> String {
    match c {
        Cannot::Repository(name) if name.is_empty() => l.pick(
            "没有可用来推导的名字".to_string(),
            "there is no name to derive it from".to_string(),
        ),
        Cannot::Repository(name) => l.pick(
            format!("名字 `{name}` 里没有 ASCII 字母或数字"),
            format!("the name `{name}` has no ASCII letter or digit"),
        ),
        Cannot::Branch(name) => l.pick(
            format!("分支名 `{name}` 里没有 ASCII 字母或数字"),
            format!("the branch name `{name}` has no ASCII letter or digit"),
        ),
    }
}

/// The refusal of `senate join` with no name, when none can be derived.
pub fn cannot_join_text(c: &Cannot, l: Locale) -> String {
    l.pick(
        format!(
            "没有给名字，这里也推不出名字：{}。请明确给出：`senate join <name>`。",
            why_cannot(c, l)
        ),
        format!(
            "No name given, and none can be derived here: {}. Give one: `senate join <name>`.",
            why_cannot(c, l)
        ),
    )
}

fn derived_source_words(d: &Derived, l: Locale) -> String {
    let r = &d.repository;
    match d.from {
        NameFrom::Origin => l.pick(
            format!("仓库 `{r}`（来自它的 origin 远程）"),
            format!("the repository `{r}` (its origin remote)"),
        ),
        NameFrom::MainWorktree => l.pick(
            format!("仓库 `{r}`（主工作区的文件夹名，没有 origin 远程）"),
            format!("the repository `{r}` (the folder of its main worktree: no origin remote)"),
        ),
        NameFrom::Directory => l.pick(
            format!("目录 `{r}`（这里不是 git 仓库）"),
            format!("the directory `{r}` (not a git repository)"),
        ),
    }
}

fn derived_branch_words(d: &Derived, l: Locale) -> String {
    match &d.branch {
        BranchPart::Added(b) => l.pick(format!("和分支 `{b}`"), format!(" and the branch `{b}`")),
        BranchPart::Default(b) => l.pick(
            format!("；默认分支 `{b}` 不加后缀"),
            format!("; the default branch `{b}` adds nothing"),
        ),
        BranchPart::Detached => l.pick(
            "；HEAD 已分离，不加后缀".to_string(),
            "; a detached HEAD adds nothing".to_string(),
        ),
        BranchPart::NoGit => String::new(),
    }
}

/// The line `senate join` with no name prints: what was derived and why.
pub fn render_derived(d: &Derived, l: Locale) -> String {
    let (id, source, branch) = (
        &d.id,
        derived_source_words(d, l),
        derived_branch_words(d, l),
    );
    l.pick(
        format!("没有给名字：由{source}{branch}推出 `{id}`。想用别的名字：`senate join <name>`。"),
        format!(
            "No name given: derived `{id}` from {source}{branch}. \
             Give a name to choose another: `senate join <name>`."
        ),
    )
}

/// The line `senate where` adds for an unbound directory.
fn would_join_line(w: &Result<Derived, Cannot>, l: Locale) -> String {
    match w {
        Ok(d) => l.pick(
            format!("不给名字的 `senate join` 在这里会叫它 `{}`。", d.id),
            format!("`senate join` with no name would call it `{}` here.", d.id),
        ),
        Err(c) => l.pick(
            format!(
                "不给名字的 `senate join` 在这里推不出名字（{}）：请给出 `senate join <name>`。",
                why_cannot(c, l)
            ),
            format!(
                "`senate join` with no name cannot derive one here ({}): give a name, \
                 `senate join <name>`.",
                why_cannot(c, l)
            ),
        ),
    }
}

/// Where `.senate/` is, for a message.
fn place_words(c: &Change, l: Locale) -> String {
    match (&c.branch, c.in_git) {
        (Some(b), _) => l.pick(
            format!("这个工作区在分支 `{b}` 上"),
            format!("this worktree on branch `{b}`"),
        ),
        (None, true) => l.pick(
            "这个工作区（HEAD 已分离）".to_string(),
            "this worktree (detached HEAD)".to_string(),
        ),
        (None, false) => l.pick("这个目录".to_string(), "this directory".to_string()),
    }
}

fn applies_line(resolved: &Resolved, l: Locale) -> String {
    match &resolved.committee {
        Some((c, src)) => l.pick(
            format!("这里现在属于 {c}（来源：{}）。", src.label(l)),
            format!("Here now: {c} (source: {}).", src.label(l)),
        ),
        None => l.pick(
            "这里现在没有绑定任何委员会。".to_string(),
            "Here now: no committee.".to_string(),
        ),
    }
}

fn follows_words(c: &Change, l: Locale) -> &'static str {
    if c.in_git {
        l.pick(
            "文件放在 .senate/，按分支保存：切换分支时会换成那个分支自己的那一份。",
            "The file is in .senate/, kept per branch: switching branches swaps in that \
             branch's own copy.",
        )
    } else {
        l.pick(
            "文件放在 .senate/（这里不是 git 仓库，没有分支）。",
            ".senate/ holds the file (this is not a git repository, so there are no branches).",
        )
    }
}

fn overridden_line(c: &Change, written: &str, l: Locale) -> Option<String> {
    let (now, src) = c.after.committee.as_ref()?;
    (now.as_str() != written).then(|| {
        l.pick(
            format!(
                "注意：{}优先于刚写入的 .senate/committee，所以这里仍是 {now}。",
                src.label(l)
            ),
            format!(
                "Note: the {} takes precedence over the .senate/committee just written, so \
                 {now} still applies.",
                src.label(l)
            ),
        )
    })
}

/// What `senate join` prints.
pub fn render_join(c: &Change, l: Locale) -> String {
    let value = c.value.clone().unwrap_or_default();
    let mut out = l.pick(
        format!("已把{}绑定到 {value}。", place_words(c, l)),
        format!("Bound {} to {value}.", place_words(c, l)),
    );
    let _ = write!(out, "\n{}", follows_words(c, l));
    let _ = write!(out, "\n{}", applies_line(&c.after, l));
    if let Some(note) = overridden_line(c, &value, l) {
        let _ = write!(out, "\n{note}");
    }
    out
}

/// What `senate leave` prints.
pub fn render_leave(c: &Change, l: Locale) -> String {
    let mut out = if c.removed {
        l.pick(
            format!("已解除{}的绑定。", place_words(c, l)),
            format!("Removed the binding of {}.", place_words(c, l)),
        )
    } else {
        l.pick(
            format!("{}本来就没有绑定。", place_words(c, l)),
            format!("There was no binding for {}.", place_words(c, l)),
        )
    };
    let _ = write!(out, "\n{}", applies_line(&c.after, l));
    out
}

fn agent_here_line(c: &Change, l: Locale) -> String {
    let claude = c.after.agents.iter().find(|a| a.runtime == Runtime::Claude);
    match claude {
        Some(a) => {
            let who = a.path.as_ref().map_or_else(
                || a.rel.to_string(),
                |p| senate_core::Address::senator(p).in_text(),
            );
            l.pick(
                format!(
                    "这里的 Claude Code 会话现在是 {who}（来源：{}）。",
                    a.source
                ),
                format!(
                    "A Claude Code session here is now {who} (source: {}).",
                    a.source
                ),
            )
        }
        None => String::new(),
    }
}

/// What `senate pin` prints.
pub fn render_pin(c: &Change, l: Locale) -> String {
    let value = c.value.clone().unwrap_or_default();
    format!(
        "{}\n{}\n{}",
        l.pick(
            format!("已让{}的会话以 {value} 的身份发言。", place_words(c, l)),
            format!("Sessions on {} now speak as {value}.", place_words(c, l)),
        ),
        follows_words(c, l),
        agent_here_line(c, l)
    )
}

/// What `senate unpin` prints.
pub fn render_unpin(c: &Change, l: Locale) -> String {
    format!(
        "{}\n{}",
        if c.removed {
            l.pick(
                format!("已取消{}的名字固定。", place_words(c, l)),
                format!("Removed the agent pin of {}.", place_words(c, l)),
            )
        } else {
            l.pick(
                format!("{}本来就没有固定名字。", place_words(c, l)),
                format!("There was no agent pin for {}.", place_words(c, l)),
            )
        },
        agent_here_line(c, l)
    )
}

/// Check `text` is an agent path and give it back normalised.
pub fn parse_pin(text: &str) -> anyhow::Result<RelPath> {
    text.parse::<RelPath>()
        .map_err(|e| anyhow::anyhow!("{text:?} is not an agent name or path: {e}"))
}

// ---- where ------------------------------------------------------------------

/// What `senate where` prints.
pub fn render_where(
    r: &Resolved,
    would_join: Option<&Result<Derived, Cannot>>,
    l: Locale,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} {}", l.pick("目录", "directory"), r.cwd.display());
    let branch = match (&r.facts.branch, r.facts.toplevel.is_some()) {
        (Some(b), _) => b.clone(),
        (None, true) => l.pick("（HEAD 已分离）", "(detached HEAD)").to_string(),
        (None, false) => l
            .pick("（不是 git 仓库）", "(not a git repository)")
            .to_string(),
    };
    let _ = writeln!(out, "{} {branch}", l.pick("分支", "branch"));
    let _ = writeln!(
        out,
        "{} {}",
        l.pick("委员会", "committee"),
        match &r.committee {
            Some((c, src)) => format!("{c} ({})", src.label(l)),
            None => l.pick("未绑定", "unbound").to_string(),
        }
    );
    for a in &r.agents {
        let who = a.path.as_ref().map_or_else(
            || a.rel.to_string(),
            |p| senate_core::Address::senator(p).in_text(),
        );
        let harness = match a.runtime {
            Runtime::Claude => "Claude Code",
            Runtime::Codex => "Codex",
        };
        let _ = writeln!(
            out,
            "{} {who} ({})",
            l.pick(format!("{harness} 会话"), format!("{harness} session")),
            a.source
        );
    }
    if r.facts.toplevel.is_some() {
        let owner = match &r.belongs_to {
            Some(SlotKey::Branch { branch }) => format!("`{branch}`"),
            Some(SlotKey::Detached) => l.pick("（detached HEAD）", "(detached HEAD)").to_string(),
            None => l
                .pick("（还没有 .senate/）", "(no .senate/ yet)")
                .to_string(),
        };
        let _ = writeln!(
            out,
            "{} {owner}",
            l.pick(".senate/ 属于", ".senate/ belongs to")
        );
        if r.others.is_empty() {
            let _ = writeln!(
                out,
                "{}",
                l.pick(
                    "这个仓库的其他分支没有保存的副本。",
                    "No saved copies for other branches of this repository."
                )
            );
        } else {
            let names: Vec<String> = r
                .others
                .iter()
                .map(|s| {
                    let name = s.meta.key.branch().map_or_else(
                        || l.pick("(detached)", "(detached)").to_string(),
                        str::to_string,
                    );
                    match &s.committee {
                        Some(c) => format!("{name} → {c}"),
                        None => name,
                    }
                })
                .collect();
            let _ = writeln!(
                out,
                "{}",
                l.pick(
                    format!(
                        "其他分支保存的副本（{} 份）：{}",
                        names.len(),
                        names.join("、")
                    ),
                    format!(
                        "Saved copies for other branches of this repository ({}): {}",
                        names.len(),
                        names.join(", ")
                    ),
                )
            );
        }
    }
    if r.committee.is_none() {
        if let Some(w) = would_join {
            let _ = writeln!(out, "{}", would_join_line(w, l));
        }
        let _ = writeln!(
            out,
            "{}",
            l.pick(
                "用 `senate join <committee>` 绑定这个工作区（在当前分支上）。",
                "`senate join <committee>` binds this worktree on this branch."
            )
        );
    }
    out
}

// ---- list -------------------------------------------------------------------

/// One saved copy, as `senate list --json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub repository: PathBuf,
    /// `None` for a detached HEAD's copy.
    pub branch: Option<String>,
    pub worktree: PathBuf,
    pub committee: Option<String>,
    pub saved_at: DateTime<Utc>,
    pub head: Option<String>,
    /// Why it has expired, if it has.
    pub stale: Option<&'static str>,
    pub dir: PathBuf,
}

fn stale_words(s: Stale) -> &'static str {
    match s {
        Stale::RepositoryGone => "repository gone",
        Stale::BranchGone => "branch gone",
        Stale::WorktreeGone => "worktree gone",
    }
}

fn stale_words_l(s: Stale, l: Locale) -> &'static str {
    match s {
        Stale::RepositoryGone => l.pick("仓库已不存在", "repository gone"),
        Stale::BranchGone => l.pick("分支已不存在", "branch gone"),
        Stale::WorktreeGone => l.pick("工作区已不存在", "worktree gone"),
    }
}

/// Every saved copy.
pub fn list(paths: &Paths) -> Vec<Row> {
    follow::slots(paths)
        .into_iter()
        .map(|s| Row {
            stale: follow::stale(&s.meta).map(stale_words),
            repository: s.meta.repository.clone(),
            branch: s.meta.key.branch().map(str::to_string),
            worktree: s.meta.worktree.clone(),
            committee: s.committee.clone(),
            saved_at: s.meta.saved_at,
            head: s.meta.head.clone(),
            dir: s.dir,
        })
        .collect()
}

pub fn render_list(rows: &[Row], l: Locale) -> String {
    if rows.is_empty() {
        return l
            .pick(
                "没有保存的 .senate/ 副本。\n",
                "No saved .senate/ copies.\n",
            )
            .to_string();
    }
    let mut out = String::new();
    let mut last: Option<&Path> = None;
    for row in rows {
        if last != Some(row.repository.as_path()) {
            if last.is_some() {
                out.push('\n');
            }
            let _ = writeln!(
                out,
                "{}{}",
                row.repository.display(),
                if row.repository.exists() {
                    ""
                } else {
                    l.pick("  （已不存在）", "  (gone)")
                }
            );
            last = Some(&row.repository);
        }
        let _ = writeln!(
            out,
            "  {:<28} {:<20} {}  {}{}",
            row.branch
                .clone()
                .unwrap_or_else(|| l.pick("(detached)", "(detached)").to_string()),
            row.committee
                .clone()
                .unwrap_or_else(|| l.pick("未绑定", "unbound").to_string()),
            row.saved_at.format("%Y-%m-%d %H:%M"),
            row.worktree.display(),
            row.stale.map_or(String::new(), |why| {
                let s = match why {
                    "repository gone" => Stale::RepositoryGone,
                    "branch gone" => Stale::BranchGone,
                    _ => Stale::WorktreeGone,
                };
                format!("  [{}: {}]", l.pick("已过期", "stale"), stale_words_l(s, l))
            })
        );
    }
    out
}

// ---- free -------------------------------------------------------------------

/// What `senate free` found.
#[derive(Debug, Default)]
pub struct FreeReport {
    pub applied: bool,
    pub slots: Vec<(Slot, Stale)>,
    pub slots_kept: usize,
    /// (state database, session id, why)
    pub sessions: Vec<(PathBuf, String, &'static str)>,
    pub agents: Vec<(PathBuf, crate::state::IdleAgent)>,
    /// Hosted Senates' data no longer in `senate.toml`, with their sizes.
    pub logs: Vec<(PathBuf, u64)>,
    pub logs_deleted: bool,
    pub after_days: u64,
    pub errors: Vec<String>,
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

fn state_dbs(paths: &Paths) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(paths.home())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("state") && n.ends_with(".db") && p.is_file())
        })
        .collect();
    out.sort();
    out
}

/// Work out what has expired and, with `apply`, free it. A hosted Senate's
/// data is a log: it is listed, and removed only with `delete_logs`.
pub fn free(
    paths: &Paths,
    settings: &Settings,
    apply: bool,
    delete_logs: bool,
) -> anyhow::Result<FreeReport> {
    let now = Utc::now();
    let mut report = FreeReport {
        applied: apply,
        after_days: settings.free_after_days,
        logs_deleted: apply && delete_logs,
        ..FreeReport::default()
    };
    // Saved copies: a copy for a branch that still exists is never stale.
    for slot in follow::slots(paths) {
        match follow::stale(&slot.meta) {
            Some(why) => report.slots.push((slot, why)),
            None => report.slots_kept += 1,
        }
    }
    // Sessions and idle agents, in each state database.
    let keep: BTreeSet<CommitteeId> = settings.committees.iter().cloned().collect();
    let ended_before = now - chrono::Duration::days(ENDED_SESSION_DAYS);
    let dead_before = now - chrono::Duration::days(DEAD_SESSION_DAYS);
    let idle_before = now
        - chrono::Duration::days(i64::try_from(settings.free_after_days).unwrap_or(i64::MAX / 4));
    let mut stores = Vec::new();
    for db in state_dbs(paths) {
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => {
                report.errors.push(format!("{}: {e}", db.display()));
                continue;
            }
        };
        let mut doomed: Vec<String> = Vec::new();
        for s in store.all_sessions()? {
            let ended = matches!(
                s.state,
                crate::session::SessionState::Ended | crate::session::SessionState::Stale
            ) && s.last_seen < ended_before;
            let dead =
                s.claude_pid.is_some_and(|p| !session::pid_alive(p)) && s.last_seen < dead_before;
            if ended || dead {
                doomed.push(s.session_id.clone());
                report.sessions.push((
                    db.clone(),
                    s.session_id,
                    if ended {
                        "ended long ago"
                    } else {
                        "its process is gone"
                    },
                ));
            }
        }
        let ignoring: BTreeSet<String> = doomed.iter().cloned().collect();
        for idle in store.idle_agents(idle_before.timestamp_millis(), &ignoring, &keep)? {
            report.agents.push((db.clone(), idle));
        }
        stores.push((db, store, doomed));
    }
    // Hosted Senates' data no longer in senate.toml.
    let hosted = settings.hosted_keys();
    if let Ok(entries) = std::fs::read_dir(paths.home().join("senates")) {
        for entry in entries.flatten().filter(|e| e.path().is_dir()) {
            let key = entry.file_name().to_string_lossy().into_owned();
            if !hosted.contains(&key) {
                report.logs.push((entry.path(), dir_size(&entry.path())));
            }
        }
        report.logs.sort();
    }
    if apply {
        for (slot, _) in &report.slots {
            if let Err(e) = std::fs::remove_dir_all(&slot.dir) {
                report.errors.push(format!("{}: {e}", slot.dir.display()));
            }
        }
        for (db, store, doomed) in &stores {
            if let Err(e) = store.delete_sessions(doomed) {
                report.errors.push(format!("{}: {e}", db.display()));
            }
            for (d, idle) in &report.agents {
                if d == db
                    && let Err(e) = store.forget_agent(&idle.agent, &idle.committees)
                {
                    report.errors.push(format!("{}: {e}", db.display()));
                }
            }
        }
        if delete_logs {
            for (dir, _) in &report.logs {
                if let Err(e) = std::fs::remove_dir_all(dir) {
                    report.errors.push(format!("{}: {e}", dir.display()));
                }
            }
        }
    }
    Ok(report)
}

fn human_bytes(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / f64::from(1 << 20))
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

pub fn render_free(r: &FreeReport, l: Locale) -> String {
    let (would, did) = (l.pick("将清理", "would free"), l.pick("已清理", "freed"));
    let verb = if r.applied { did } else { would };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}",
        l.pick("保存的 .senate/ 副本", "Saved .senate/ copies"),
        l.pick(
            format!(
                "{verb} {} 份，保留 {} 份（分支还在的不会过期）",
                r.slots.len(),
                r.slots_kept
            ),
            format!(
                "{verb} {}, kept {} (a copy whose branch still exists never expires)",
                r.slots.len(),
                r.slots_kept
            )
        )
    );
    for (slot, why) in &r.slots {
        let _ = writeln!(
            out,
            "  {} {}  [{}]",
            slot.meta.repository.display(),
            slot.meta
                .key
                .branch()
                .map_or_else(|| "(detached)".to_string(), str::to_string),
            stale_words_l(*why, l)
        );
    }
    let _ = writeln!(
        out,
        "{} {}",
        l.pick("会话记录", "Session records"),
        l.pick(
            format!(
                "{verb} {} 条（结束超过 {ENDED_SESSION_DAYS} 天，或进程已不在且超过 {DEAD_SESSION_DAYS} 天）",
                r.sessions.len()
            ),
            format!(
                "{verb} {} (ended over {ENDED_SESSION_DAYS} days ago, or process gone for over \
                 {DEAD_SESSION_DAYS} day)",
                r.sessions.len()
            )
        )
    );
    let items: usize = r.agents.iter().map(|(_, a)| a.items).sum();
    let cursors: usize = r.agents.iter().map(|(_, a)| a.committees.len()).sum();
    let _ = writeln!(
        out,
        "{} {}",
        l.pick("长期没有联系的 agent", "Agents with no session and no contact"),
        l.pick(
            format!(
                "{verb} {} 个 agent 的 {cursors} 个游标和 {items} 条排队项（超过 {} 天；senate.toml 的 free_after_days）",
                r.agents.len(),
                r.after_days
            ),
            format!(
                "{verb} {cursors} cursors and {items} queued items of {} agents (over {} days; \
                 `free_after_days` in senate.toml)",
                r.agents.len(),
                r.after_days
            )
        )
    );
    for (_, idle) in &r.agents {
        let _ = writeln!(
            out,
            "  {}  {}",
            senate_core::Address::senator(&idle.agent).in_text(),
            idle.committees
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if r.logs.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            l.pick(
                "senate.toml 里没有的 Senate：没有数据目录。",
                "Hosted Senates no longer in senate.toml: none."
            )
        );
    } else {
        let _ = writeln!(
            out,
            "{}",
            if r.logs_deleted {
                l.pick(
                    "senate.toml 里已没有的 Senate，日志已删除：",
                    "Hosted Senates no longer in senate.toml, logs deleted:",
                )
            } else {
                l.pick(
                    "senate.toml 里已没有的 Senate 的数据（这是日志：没有 --delete-logs 就不会删，保留）：",
                    "Hosted Senates no longer in senate.toml (a log: kept; only --yes \
                     --delete-logs removes it):",
                )
            }
        );
        for (dir, size) in &r.logs {
            let _ = writeln!(out, "  {}  {}", dir.display(), human_bytes(*size));
        }
    }
    for e in &r.errors {
        let _ = writeln!(out, "{}: {e}", l.pick("出错", "error"));
    }
    if !r.applied {
        let _ = writeln!(
            out,
            "\n{}",
            l.pick(
                "这只是预演，什么都没有改动。加 --yes 才会执行。",
                "This is a dry run; nothing was changed. Add --yes to do it."
            )
        );
    }
    out
}

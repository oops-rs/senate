//! The read-only lookups an agent-mode run holds besides the log tools:
//! `committee_brief`, `committee_status`, `presence`, `tasks` and `checkpoint`.
//!
//! Like `log_read` / `log_search` they are native tools over the [`Hub`],
//! bound to one committee by construction, and none has a side effect: each is
//! one or two hub reads rendered as text the model can cite. They are
//! registered on every workspace but offered only to `converse` runs (the
//! run's [`RunProfile`](basis::RunProfile) names them); the workspace's own
//! roster stays the two log tools, so `interpret`, `answer_on_behalf`,
//! `brief_top`, `triage` and `pick_agent` never see them.
//!
//! Presence, checkpoints and task threads are kept per agent, and a person
//! may have several agents, so these tools answer per agent, by its own
//! name (`claude`). Senators in their output are bracketed mentions:
//! `[senator::alice]` for a person, `[senator::claude]` for an agent.
//! Their text is read by the model only, and is English.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use basis::async_trait;
use basis::tools::{
    ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
};
use senate_core::{
    Address, AgentOf, CommitteeId, Hub, Locale, PresenceState, Roster, SenatorName, Senior,
    StatusState,
};
use serde_json::{Value, json};

use super::tools::{MAX_LIMIT, descriptor, limit, unavailable};
use super::{CHECKPOINT, COMMITTEE_BRIEF, COMMITTEE_STATUS, PRESENCE, TASKS};

/// Characters of the brief one `committee_brief` call returns.
const BRIEF_CHARS: usize = 6_000;
/// Characters of each checkpoint file `checkpoint` returns.
const CHECKPOINT_FILE_CHARS: usize = 1_500;

fn bound(committee: Option<&CommitteeId>) -> Result<&CommitteeId, String> {
    committee.ok_or_else(|| {
        "This group is not bound to a committee yet, so it has no status to look up.".to_owned()
    })
}

fn no_arguments() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "required": [],
        "additionalProperties": false
    })
}

fn cut(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// A senator as the model may write one: a person's handle or a name they
/// go by (`alice`, `Alice`, `@alice`), an agent's name (`claude`), or
/// either as an address or mention (`senator::claude`, `[senator::claude]`).
/// The roster says which it is ([`Roster::is_person`]).
fn senator_of(raw: &str, roster: &Roster) -> Option<SenatorName> {
    let raw = raw
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let bare = raw.strip_prefix("senator::").unwrap_or(raw);
    roster
        .resolve(bare)
        .map(Senior::path)
        .or_else(|| bare.trim_start_matches('@').parse().ok())
}

fn mention(path: &SenatorName) -> String {
    Address::senator(path).in_text()
}

fn mentions<'a>(paths: impl IntoIterator<Item = &'a SenatorName>) -> String {
    paths
        .into_iter()
        .map(mention)
        .collect::<Vec<_>>()
        .join(", ")
}

fn state_word(state: StatusState) -> &'static str {
    match state {
        StatusState::Processing => "processing",
        StatusState::Active => "active",
        StatusState::Suspended => "suspended",
        StatusState::Archived => "archived",
    }
}

fn parse_state(raw: &str) -> Option<StatusState> {
    match raw {
        "processing" => Some(StatusState::Processing),
        "active" => Some(StatusState::Active),
        "suspended" => Some(StatusState::Suspended),
        "archived" => Some(StatusState::Archived),
        _ => None,
    }
}

/// The fields every agent tool holds: the hub and the one committee it reads.
struct Scope {
    hub: Arc<dyn Hub>,
    committee: Option<CommitteeId>,
}

impl Scope {
    async fn roster(&self) -> Result<Roster, String> {
        self.hub.roster().await.map_err(unavailable)
    }
}

macro_rules! agent_tool {
    ($name:ident) => {
        pub(crate) struct $name(Scope);

        impl $name {
            pub(crate) fn new(hub: Arc<dyn Hub>, committee: Option<CommitteeId>) -> Self {
                Self(Scope { hub, committee })
            }
        }
    };
}

agent_tool!(CommitteeBrief);
agent_tool!(CommitteeStatus);
agent_tool!(Presence);
agent_tool!(Tasks);
agent_tool!(Checkpoint);

impl ToolDefinition for CommitteeBrief {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            COMMITTEE_BRIEF,
            "This committee's brief, built from its whole log: the top of mind, the decisions in \
             force (with seq), open questions, the bound groups, each agent's latest checkpoint \
             and presence (one person's agents each have their own), and who has been active.",
            no_arguments(),
        )
    }
}

#[async_trait]
impl ToolExecutor for CommitteeBrief {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        let committee = bound(self.0.committee.as_ref())?;
        let brief = self.0.hub.brief(committee).await.map_err(unavailable)?;
        Ok(brief.to_markdown(Locale::En, BRIEF_CHARS))
    }
}

impl ToolDefinition for CommitteeStatus {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            COMMITTEE_STATUS,
            "Which groups this committee is bound to, and which agents are connected right now. \
             Pass `agents` to check some: a person's handle checks every agent seated under \
             them, an agent's name (claude) that one agent. null checks every agent known \
             to the committee (its presence, checkpoints and recent writers).",
            committee_status_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for CommitteeStatus {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = bound(self.0.committee.as_ref())?;
        let brief = self.0.hub.brief(committee).await.map_err(unavailable)?;
        let asked: Vec<&str> = input
            .get("agents")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut agents: BTreeSet<SenatorName> = BTreeSet::new();
        let mut agentless: Vec<SenatorName> = Vec::new();
        let mut unknown: Vec<&str> = Vec::new();
        let roster = self.0.roster().await?;
        if asked.is_empty() {
            let people = roster.people();
            let present = brief.presence.iter().map(|p| p.agent.clone());
            let wrote = brief
                .participants
                .iter()
                .filter_map(|p| people.as_agent(&p.address).cloned());
            let checkpointed = brief.checkpoints.iter().map(|c| c.agent.clone());
            agents.extend(present.chain(wrote).chain(checkpointed));
        } else {
            for raw in asked {
                match senator_of(raw, &roster) {
                    Some(agent) if !roster.is_person(&agent) => {
                        agents.insert(agent);
                    }
                    Some(person) => {
                        let seated = roster.agents_of(person.name());
                        if seated.is_empty() {
                            agentless.push(person);
                        }
                        agents.extend(seated.into_iter().cloned());
                    }
                    None => unknown.push(raw),
                }
            }
        }

        let mut out = format!("committee {committee}\nBound groups:");
        if brief.groups.is_empty() {
            out.push_str(" none");
        }
        for group in &brief.groups {
            let name = group.group_name.as_deref().unwrap_or(&group.group_id);
            let _ = write!(out, "\n- {name} ({}:{})", group.platform, group.group_id);
        }
        let (mut online, mut offline) = (Vec::new(), Vec::new());
        for agent in &agents {
            if self.0.hub.is_agent_connected(committee, agent).await {
                online.push(agent);
            } else {
                offline.push(agent);
            }
        }
        let list = |paths: &[&SenatorName]| {
            if paths.is_empty() {
                "none".to_owned()
            } else {
                mentions(paths.iter().copied())
            }
        };
        let _ = write!(
            out,
            "\nOnline agents: {}\nOffline agents: {}",
            list(&online),
            list(&offline)
        );
        if !agentless.is_empty() {
            let _ = write!(out, "\nNo agent seated: {}", mentions(&agentless));
        }
        if !unknown.is_empty() {
            let _ = write!(out, "\nNot on the roster: {}", unknown.join(", "));
        }
        Ok(out)
    }
}

impl ToolDefinition for Presence {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            PRESENCE,
            "Each agent's presence on this committee, per agent (one person's agents each have \
             their own): online, disconnected, absent (lease expired), taken over by the \
             Speaker, or reassigned (and to whom), since when, and how far behind the log it is.",
            no_arguments(),
        )
    }
}

#[async_trait]
impl ToolExecutor for Presence {
    async fn execute(&self, _ctx: ParallelToolContext, _input: Value) -> ToolResult {
        let committee = bound(self.0.committee.as_ref())?;
        let page = self.0.hub.presence(committee).await.map_err(unavailable)?;
        if page.agents.is_empty() {
            return Ok("(no agent has any presence record yet)".to_owned());
        }
        let mut out = format!("committee {committee}, log at seq {}:", page.head);
        for p in &page.agents {
            let _ = write!(
                out,
                "\n- {} · {} · since {} (seq {})",
                mention(&p.agent),
                p.state.label(Locale::En),
                p.since.format("%Y-%m-%d %H:%M UTC"),
                p.since_seq
            );
            if let (PresenceState::Reassigned, Some(owner)) = (p.state, &p.new_owner) {
                let _ = write!(out, " · handed to {}", mention(owner));
            }
            if let Some(lag) = p.lag.filter(|l| *l > 0) {
                let _ = write!(out, " · {lag} events behind");
            }
        }
        Ok(out)
    }
}

impl ToolDefinition for Tasks {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            TASKS,
            "This committee's task threads (questions and requests to agents, and work runs), \
             longest in their state first: root seq, who asked whom, title, state, owner, and \
             whether it was answered, escalated or taken over. A task belongs to the agent it \
             was addressed to, not to that person's other agents.",
            tasks_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for Tasks {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = bound(self.0.committee.as_ref())?;
        let state = match input.get("state").and_then(Value::as_str) {
            Some(raw) => Some(parse_state(raw).ok_or_else(|| format!("unknown state {raw:?}"))?),
            None => None,
        };
        let only = match input
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
        {
            Some(raw) => {
                let roster = self.0.roster().await?;
                Some(senator_of(raw, &roster).ok_or_else(|| {
                    format!(
                        "not a senator: {raw:?}; write a person's handle or an agent's name \
                         such as claude"
                    )
                })?)
            }
            None => None,
        };
        let board = self.0.hub.tasks(committee).await.map_err(unavailable)?;
        let mut threads = board.threads(state);
        if let Some(only) = &only {
            // An agent: its own threads. A person: those of any of their agents.
            let roster = self.0.roster().await?;
            let owner = roster.is_person(only).then(|| only.name());
            threads.retain(|t| {
                t.agents()
                    .any(|a| a == only || (owner.is_some() && roster.owner_of(a) == owner))
            });
        }
        if threads.is_empty() {
            return Ok("(no such tasks)".to_owned());
        }
        let mut out = String::new();
        for t in threads.into_iter().take(limit(&input)) {
            let to: Vec<String> = t.to.iter().map(Address::in_text).collect();
            let _ = write!(
                out,
                "seq {} · {} · {} → {} · {} (since seq {}) · {}",
                t.root,
                t.kind,
                t.on_behalf_of.as_ref().unwrap_or(&t.from).in_text(),
                to.join(", "),
                state_word(t.state),
                t.since,
                cut(&t.title, 80),
            );
            if let Some(owner) = &t.owner {
                let _ = write!(
                    out,
                    " · owner {} (session {})",
                    mention(&owner.agent),
                    owner.session
                );
            }
            out.push_str(if t.answered {
                " · answered"
            } else {
                " · not answered"
            });
            if let Some(to) = t.escalated_now() {
                let _ = write!(out, " · escalated to {}", Address::senior(to).in_text());
            }
            if let Some(by) = &t.takeover {
                let _ = write!(out, " · taken over by {}", Address::senior(by).in_text());
            }
            out.push('\n');
        }
        Ok(out.trim_end().to_owned())
    }
}

impl ToolDefinition for Checkpoint {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            CHECKPOINT,
            "One agent's latest checkpoint on this committee: its seq and time, branch and commit, \
             whether it was pushed, the summary, and its PROGRESS.md, DECISIONS.md and OPEN.md. \
             A checkpoint is one agent's own; name the agent (claude). A person's handle \
             is accepted only when that person has exactly one agent; otherwise the candidates \
             are listed and you call again with one of them.",
            checkpoint_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for Checkpoint {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = bound(self.0.committee.as_ref())?;
        let raw = input
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let roster = self.0.roster().await?;
        let senator = senator_of(raw, &roster).ok_or_else(|| {
            format!("not a senator: {raw:?}; write an agent's name such as claude")
        })?;
        let agent = if !roster.is_person(&senator) {
            senator
        } else {
            let person = mention(&senator);
            match crate::engine::agent_of(&roster, senator.name()) {
                AgentOf::One(agent) => agent,
                AgentOf::Several(candidates) => {
                    return Err(format!(
                        "{person} has several agents here: {}. A checkpoint is one agent's own: \
                         call checkpoint again with one of them.",
                        mentions(&candidates)
                    ));
                }
                AgentOf::None(_) => {
                    return Ok(format!(
                        "{person} has no agent seated here, so there is no checkpoint."
                    ));
                }
            }
        };
        let Some(record) = self
            .0
            .hub
            .latest_checkpoint(committee, &agent)
            .await
            .map_err(unavailable)?
        else {
            return Ok(format!(
                "{} has no checkpoint on {committee} yet.",
                mention(&agent)
            ));
        };
        let payload = &record.checkpoint;
        let mut out = format!(
            "Latest checkpoint of {}: seq {} · {} · {} · {}\n{}",
            mention(&agent),
            record.seq,
            record.ts.format("%Y-%m-%d %H:%M UTC"),
            payload.at(),
            if payload.pushed {
                "pushed"
            } else {
                "not pushed"
            },
            cut(&record.body, 300),
        );
        for (name, text) in [
            ("PROGRESS.md", &payload.files.progress),
            ("DECISIONS.md", &payload.files.decisions),
            ("OPEN.md", &payload.files.open),
        ] {
            let text = text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map_or_else(
                    || "(no such file)".to_owned(),
                    |t| cut(t, CHECKPOINT_FILE_CHARS),
                );
            let _ = write!(out, "\n\n### {name}\n{text}");
        }
        Ok(out)
    }
}

fn committee_status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agents": {
                "type": ["array", "null"],
                "items": { "type": "string" },
                "description": "Whom to check: a person's handle checks every agent seated under them (e.g. \"alice\"), an agent's name checks that one agent (e.g. \"codex\"). null for every agent known to the committee."
            }
        },
        "required": ["agents"],
        "additionalProperties": false
    })
}

fn tasks_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "state": {
                "type": ["string", "null"],
                "enum": ["processing", "active", "suspended", "archived", null],
                "description": "Only threads in this state; null for all. processing: new or in progress, not answered yet; active: answered, still open; suspended: parked on a person; archived: closed."
            },
            "agent": {
                "type": ["string", "null"],
                "description": "Only threads addressed to this agent (an agent's name, e.g. \"claude\"), or to any agent of this person (a handle, e.g. \"alice\"); null for every thread."
            },
            "limit": {
                "type": ["integer", "null"],
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "How many threads, at most (null for 20)."
            }
        },
        "required": ["state", "agent", "limit"],
        "additionalProperties": false
    })
}

fn checkpoint_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent": {
                "type": "string",
                "minLength": 1,
                "description": "The agent whose checkpoint to read, by its name, e.g. \"claude\". A person's handle (e.g. \"alice\") only when that person has exactly one agent."
            }
        },
        "required": ["agent"],
        "additionalProperties": false
    })
}

#[cfg(test)]
pub(super) fn schemas() -> Vec<Value> {
    vec![
        no_arguments(),
        committee_status_schema(),
        tasks_schema(),
        checkpoint_schema(),
    ]
}

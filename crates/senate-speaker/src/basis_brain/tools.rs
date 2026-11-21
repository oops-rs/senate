//! `log_read` and `log_search`: the only tools the Speaker's model holds.
//!
//! Native tools (`WorkspaceBuilder::with_tool`) over the [`Hub`], scoped to
//! one committee by construction: each workspace is opened with tools bound to
//! its own committee, so a model cannot read another committee's log by asking.
//! Neither reads the whole log: `log_read` reads one page, `log_search` is
//! [`Hub::search`] (an index, once the store has one).

use std::sync::Arc;

use basis::async_trait;
use basis::tools::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolDefinition,
    ToolDurability, ToolExecutionCategory, ToolExecutor, ToolResult, ToolSideEffectLevel,
};
use senate_core::{CommitteeId, Event, Hub, HubError, Seq};
use serde_json::{Value, json};

use super::prompt::render_event;
use super::{LOG_READ, LOG_SEARCH, MEMORY_SEARCH};
use crate::engine::CURSOR;

const DEFAULT_LIMIT: usize = 20;
pub(super) const MAX_LIMIT: usize = 50;

/// Providers run these tools in strict mode, which rejects a schema unless
/// every property is in `required`: an optional argument is a nullable one.
pub(super) fn descriptor(name: &str, description: &str, schema: Value) -> RuntimeToolDescriptor {
    RuntimeToolDescriptor::builder(name)
        .description(description)
        .input_schema(schema)
        .side_effect_level(ToolSideEffectLevel::None)
        .durability(ToolDurability::ReplaySafe)
        .execution_category(ToolExecutionCategory::ReadOnlyParallel)
        .approval_category(ToolApprovalCategory::ReadOnly)
        .build()
}

pub(super) fn limit(input: &Value) -> usize {
    input
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |n| usize::try_from(n).unwrap_or(MAX_LIMIT))
        .clamp(1, MAX_LIMIT)
}

pub(super) fn committee_or_err(committee: Option<&CommitteeId>) -> Result<&CommitteeId, String> {
    committee.ok_or_else(|| {
        "This group is not bound to a committee yet, so there is no log to read.".to_owned()
    })
}

pub(super) fn unavailable(error: HubError) -> String {
    format!("log unavailable: {error}")
}

fn render(events: &[Event], empty: &str) -> String {
    if events.is_empty() {
        return empty.to_owned();
    }
    events
        .iter()
        .map(render_event)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The committee's last seq, found with point reads: seqs are dense from 1,
/// so gallop up from `hint` to a seq that does not exist, then bisect.
/// About `2·log2(distance)` reads, never a scan.
async fn head(hub: &dyn Hub, committee: &CommitteeId, hint: Seq) -> Result<Seq, HubError> {
    // `low` exists (or is 0); `high` does not.
    let mut low = 0u64;
    let mut probe = hint.0.max(1);
    let mut step = 1u64;
    let mut high = loop {
        if hub.get(committee, Seq(probe)).await?.is_none() {
            break probe;
        }
        low = probe;
        probe = probe.saturating_add(step);
        step = step.saturating_mul(2);
    };
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if hub.get(committee, Seq(middle)).await?.is_some() {
            low = middle;
        } else {
            high = middle;
        }
    }
    Ok(Seq(low))
}

/// The last `limit` events of the committee, oldest first.
async fn tail(
    hub: &dyn Hub,
    committee: &CommitteeId,
    limit: usize,
) -> Result<Vec<Event>, HubError> {
    // The Speaker's cursor is at or near the head; a missing one is a guess
    // like any other.
    let hint = hub.cursor(committee, CURSOR).await.unwrap_or(Seq::ZERO);
    let last = head(hub, committee, hint).await?;
    let after = Seq(last.0.saturating_sub(limit as u64));
    hub.read(committee, after, limit).await
}

pub(crate) struct LogRead {
    hub: Arc<dyn Hub>,
    committee: Option<CommitteeId>,
}

impl LogRead {
    pub(crate) fn new(hub: Arc<dyn Hub>, committee: Option<CommitteeId>) -> Self {
        Self { hub, committee }
    }
}

impl ToolDefinition for LogRead {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            LOG_READ,
            "Read this committee's shared log in seq order. With `after`, returns the events \
             following that seq (oldest first); without it, the most recent events. Each line \
             shows seq, time, kind, who wrote it (a bracketed mention: [senator::alice] is a \
             person, [senator::claude] one of their agents, [speaker::claude] the \
             Speaker answering for that agent), recipients, thread, and the body.",
            log_read_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for LogRead {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = committee_or_err(self.committee.as_ref())?;
        let limit = limit(&input);
        let events = match input.get("after").and_then(Value::as_u64) {
            Some(after) => self.hub.read(committee, Seq(after), limit).await,
            None => tail(self.hub.as_ref(), committee, limit).await,
        }
        .map_err(unavailable)?;
        Ok(render(&events, "(no more events)"))
    }
}

pub(crate) struct LogSearch {
    hub: Arc<dyn Hub>,
    committee: Option<CommitteeId>,
}

impl LogSearch {
    pub(crate) fn new(hub: Arc<dyn Hub>, committee: Option<CommitteeId>) -> Self {
        Self { hub, committee }
    }
}

impl ToolDefinition for LogSearch {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            LOG_SEARCH,
            "Search this committee's shared log for events whose text contains `query` \
             (case-insensitive). Returns the most recent matches, oldest first. Use short \
             distinctive words: a number, a name, a term.",
            log_search_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for LogSearch {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = committee_or_err(self.committee.as_ref())?;
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| "query is required".to_owned())?;
        let mut events = self
            .hub
            .search(committee, query, limit(&input))
            .await
            .map_err(unavailable)?;
        // Newest first from the hub; the model reads in log order.
        events.reverse();
        Ok(render(&events, "(no matching events)"))
    }
}

pub(crate) struct MemorySearch {
    hub: Arc<dyn Hub>,
    committee: Option<CommitteeId>,
}

impl MemorySearch {
    pub(crate) fn new(hub: Arc<dyn Hub>, committee: Option<CommitteeId>) -> Self {
        Self { hub, committee }
    }
}

impl ToolDefinition for MemorySearch {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            MEMORY_SEARCH,
            "Search what this committee knows: short points distilled from its discussion, each \
             with the seqs it came from, newer ones replacing older. Look here first; use \
             log_search for what is recent or not distilled yet.",
            memory_search_schema(),
        )
    }
}

#[async_trait]
impl ToolExecutor for MemorySearch {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let committee = committee_or_err(self.committee.as_ref())?;
        let query = input.get("query").and_then(Value::as_str).unwrap_or("");
        let hits = self
            .hub
            .memory_search(committee, query, limit(&input), false)
            .await
            .map_err(unavailable)?;
        Ok(render_memory(&hits))
    }
}

/// Memory points as lines a model can cite.
pub(crate) fn render_memory(hits: &[senate_core::MemoryHit]) -> String {
    if hits.is_empty() {
        return "(nothing known about that)".to_owned();
    }
    hits.iter()
        .map(|hit| {
            let refs: Vec<String> = hit.point.refs.iter().map(ToString::to_string).collect();
            let by = if hit.point.by_person() {
                format!(", corrected by {}", hit.point.by.in_text())
            } else {
                String::new()
            };
            format!(
                "- seq {} (from seq {}{by}): {}",
                hit.point.seq,
                refs.join(", "),
                hit.point.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn memory_search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to look for, in the language they were written in, e.g. \"release day\" or \"发布\". Empty for the newest points."
            },
            "limit": {
                "type": ["integer", "null"],
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "How many points, at most (null for 20)."
            }
        },
        "required": ["query", "limit"],
        "additionalProperties": false
    })
}

fn log_read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "after": {
                "type": ["integer", "null"],
                "minimum": 0,
                "description": "Return events with seq greater than this. null for the latest events."
            },
            "limit": {
                "type": ["integer", "null"],
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "How many events, at most (null for 20)."
            }
        },
        "required": ["after", "limit"],
        "additionalProperties": false
    })
}

fn log_search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "Text to look for, in the language it was written in, e.g. \"refund cap\", \"退款上限\" or \"Idempotency\"."
            },
            "limit": {
                "type": ["integer", "null"],
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "How many matches, at most (null for 20)."
            }
        },
        "required": ["query", "limit"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use senate_core::{Address, Kind, NewEvent};

    use super::*;
    use crate::tests::hub::MemoryHub;

    /// Strict tool mode (what the provider applies) rejects a schema with a
    /// property that is not required: the Speaker's answers failed with
    /// HTTP 400 before this held.
    #[test]
    fn every_property_is_required_for_strict_mode() {
        for schema in [
            log_read_schema(),
            log_search_schema(),
            memory_search_schema(),
        ] {
            let required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for key in schema["properties"].as_object().expect("properties").keys() {
                assert!(required.contains(&key.as_str()), "{key} in {schema}");
            }
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    #[tokio::test]
    async fn the_head_is_found_from_any_hint_without_a_scan() {
        let hub = MemoryHub::new();
        let committee = CommitteeId::new("refund-v2").expect("committee");
        assert_eq!(
            head(hub.as_ref(), &committee, Seq(7)).await.expect("head"),
            Seq::ZERO
        );
        for n in 1..=37 {
            hub.append(NewEvent::new(
                committee.clone(),
                Kind::Note,
                Address::Speaker,
                format!("n{n}"),
            ))
            .await
            .expect("append");
        }
        for hint in [0, 1, 5, 36, 37, 38, 1_000] {
            hub.reset_counts();
            assert_eq!(
                head(hub.as_ref(), &committee, Seq(hint))
                    .await
                    .expect("head"),
                Seq(37),
                "hint {hint}"
            );
            assert!(hub.gets() <= 24, "hint {hint}: {} point reads", hub.gets());
        }
        hub.reset_counts();
        let last = tail(hub.as_ref(), &committee, 3).await.expect("tail");
        let bodies: Vec<_> = last.iter().map(|e| e.body.as_str()).collect();
        assert_eq!(bodies, ["n35", "n36", "n37"]);
        assert_eq!(hub.reads(), 1, "one page read, not the log");
    }
}

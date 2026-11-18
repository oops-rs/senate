use rmcp::schemars::JsonSchema;
use senate_core::secretary_work::{SecretaryWorkOp, SecretaryWorkUpdate, WorkThread};
use senate_core::{CommitteeId, EventId, Seq, Step};
use serde::Deserialize;

use super::{Request, SessionSelector, StepParam, parse_senator};

pub(super) const HELP: &str = "Read or record work explicitly shared by this session's secretary team. Actions are members, list, read, create, claim, assign, progress, note and handoff. Use principal when this session belongs to several teams. Read needs item and can resume at the returned next offset. List can resume after the returned next ID. On create use both committee and thread to link a source. On handoff use text for the next member and omit to to leave the item unclaimed. Use the stable senator ID for to. Work text is written by team members.";

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SecretaryWorkParams {
    pub action: String,
    pub principal: Option<String>,
    pub item: Option<String>,
    /// An operation ID to reuse when retrying the same change.
    pub id: Option<String>,
    pub title: Option<String>,
    pub committee: Option<String>,
    pub thread: Option<u64>,
    pub state: Option<String>,
    pub steps: Option<Vec<StepParam>>,
    pub text: Option<String>,
    pub to: Option<String>,
    pub offset: Option<usize>,
    pub after: Option<String>,
}

fn parse_id(raw: &str) -> Result<EventId, String> {
    raw.parse::<EventId>().map_err(|e| e.to_string())
}

impl SecretaryWorkParams {
    pub(super) fn request(self, session: SessionSelector) -> Result<Request, String> {
        if session.session_id.is_none() && session.claude_pid.is_none() {
            return Err("secretary work requires a registered coding session".into());
        }
        let principal = self.principal.as_deref().map(parse_senator).transpose()?;
        let item = || parse_id(self.item.as_deref().ok_or("name the work item")?);
        let id = || {
            Ok::<_, String>(
                self.id
                    .as_deref()
                    .map(parse_id)
                    .transpose()?
                    .unwrap_or_default(),
            )
        };
        let operation = match self.action.as_str() {
            "members" => SecretaryWorkOp::Members,
            "list" => SecretaryWorkOp::List {
                after: self.after.as_deref().map(parse_id).transpose()?,
            },
            "read" => SecretaryWorkOp::Read {
                item: item()?,
                offset: self.offset.unwrap_or_default(),
            },
            "create" => {
                let thread = match (self.committee, self.thread) {
                    (Some(committee), Some(root)) => Some(WorkThread {
                        committee: CommitteeId::new(committee).map_err(|e| e.to_string())?,
                        root: Seq(root),
                    }),
                    (None, None) => None,
                    _ => return Err("name both the committee and the source thread".into()),
                };
                SecretaryWorkOp::Create {
                    id: id()?,
                    title: self.title.ok_or("provide a title")?,
                    thread,
                }
            }
            action => {
                let update = match action {
                    "claim" => SecretaryWorkUpdate::Claim,
                    "assign" => SecretaryWorkUpdate::Assign {
                        to: parse_senator(self.to.as_deref().ok_or("name the assignee")?)?,
                    },
                    "progress" => SecretaryWorkUpdate::Progress {
                        state: serde_json::from_value(serde_json::Value::String(
                            self.state.ok_or("provide the state")?,
                        ))
                        .map_err(|_| "state is processing, active, suspended or archived")?,
                        steps: self.steps.map(|steps| {
                            steps
                                .into_iter()
                                .map(|step| Step {
                                    id: step.id,
                                    label: step.label,
                                    done: step.done,
                                })
                                .collect()
                        }),
                    },
                    "note" => SecretaryWorkUpdate::Note {
                        text: self.text.ok_or("provide the note text")?,
                    },
                    "handoff" => SecretaryWorkUpdate::Handoff {
                        to: self.to.as_deref().map(parse_senator).transpose()?,
                        text: self.text.ok_or("provide the handoff note")?,
                    },
                    _ => return Err(
                        "action is members, list, read, create, claim, assign, progress, note or handoff"
                            .into(),
                    ),
                };
                SecretaryWorkOp::Change {
                    item: item()?,
                    id: id()?,
                    update,
                }
            }
        };
        Ok(Request::SecretaryWork {
            session,
            principal,
            operation,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_needs_a_session_and_links_the_qualified_source_thread() {
        let params = || -> SecretaryWorkParams {
            serde_json::from_value(serde_json::json!({
                "action": "create", "title": "Review queue", "committee": "delivery", "thread": 1
            }))
            .unwrap()
        };
        assert!(params().request(SessionSelector::default()).is_err());
        let session = SessionSelector {
            session_id: Some("native-session".into()),
            claude_pid: None,
        };
        let request = params().request(session.clone()).unwrap();
        let Request::SecretaryWork {
            session: selected,
            operation:
                SecretaryWorkOp::Create {
                    thread: Some(source),
                    ..
                },
            ..
        } = request
        else {
            panic!("{request:?}")
        };
        assert_eq!(selected, session);
        assert_eq!(source.committee.as_str(), "delivery");
        assert_eq!(source.root, Seq(1));
    }
}

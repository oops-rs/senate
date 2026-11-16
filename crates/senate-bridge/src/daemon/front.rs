//! One daemon, several Senates.
//!
//! The daemon keeps one [`Shared`] per Senate it serves, each with its own
//! server, token, state database, streams, cursors, heartbeats, queue and
//! sessions: nothing one Senate keeps is seen by another. [`Front`] is what
//! the local socket talks to. It sends each request to the Senate it
//! concerns:
//!
//! - a hook to the Senate the session's agent path selects
//!   ([`crate::agent`]): `alice/claude` to the one where alice's agents sit
//!   directly, `alice/claude/reviewer` to the one `alice/claude` presides
//!   over. A session that moves to another Senate (its pin changed) is
//!   ended in the one it left;
//! - a request from a session to the Senate the session is registered in;
//! - a presiding request (`docket`, `route`, `cover`, `refer`) to the
//!   Senate whose Speaker's seat the session's agent holds. So a presiding
//!   session is attached to two Senates: a senator in one, the Speaker of
//!   the other (or both in one, for a top Senate used alone);
//! - anything else (no session, the desktop app's requests) to the primary
//!   Senate: the one where the person's agents sit directly.
//!
//! With one Senate, every request goes to it, exactly as before there were
//! several.

use super::*;
use crate::ipc::{DocketReply, PresidesReply, Presiding, SenateDocket};

pub(crate) struct Front {
    senates: Vec<Arc<Shared>>,
    relay: Arc<Relay>,
    watch: broadcast::Sender<WatchEvent>,
    /// Several Senates: agent paths are placed under the person, and
    /// select the Senate.
    several: bool,
}

impl Front {
    pub(crate) fn new(
        senates: Vec<Arc<Shared>>,
        relay: Arc<Relay>,
        watch: broadcast::Sender<WatchEvent>,
        several: bool,
    ) -> Self {
        Self {
            senates,
            relay,
            watch,
            several,
        }
    }

    pub(crate) fn senates(&self) -> &[Arc<Shared>] {
        &self.senates
    }

    pub(crate) fn relay(&self) -> &Arc<Relay> {
        &self.relay
    }

    pub(crate) fn watch(&self) -> &broadcast::Sender<WatchEvent> {
        &self.watch
    }

    /// The Senate where the person's agents sit directly, else the first.
    fn primary(&self) -> &Arc<Shared> {
        self.senates
            .iter()
            .find(|s| s.seating().presider().is_none())
            .unwrap_or(&self.senates[0])
    }

    /// The Senate `agent` sits in: the one whose agents sit directly under
    /// `agent`'s parent path.
    fn senate_of(&self, agent: &SenatorPath) -> Option<&Arc<Shared>> {
        let parent = agent.presider().unwrap_or_else(|| agent.senior_path());
        self.senates.iter().find(|s| s.seating().base() == parent)
    }

    /// The Senate whose Speaker's seat `agent` holds.
    fn seat_of(&self, agent: &SenatorPath) -> Option<&Arc<Shared>> {
        self.senates.iter().find(|s| s.presided_by(agent))
    }

    /// The Senate a session is registered in, and the session: when it is
    /// registered in several (it moved), the most recently seen.
    fn locate(&self, selector: &SessionSelector) -> Option<(Arc<Shared>, Session)> {
        if selector.session_id.is_none() && selector.claude_pid.is_none() {
            return None;
        }
        self.senates
            .iter()
            .filter_map(|s| {
                let session = s.resolve(selector).ok()?;
                (session.state != SessionState::Ended).then(|| (s.clone(), session))
            })
            .max_by_key(|(_, session)| session.last_seen)
    }

    /// The daemon's status: the primary Senate's, with the others under it.
    pub(crate) fn status(&self) -> Result<StatusReply, IpcError> {
        let mut status = self.primary().on_status()?;
        for shared in &self.senates {
            if !Arc::ptr_eq(shared, self.primary()) {
                status.others.push(shared.on_status()?);
            }
        }
        Ok(status)
    }

    pub(crate) async fn dispatch(self: &Arc<Self>, request: Request) -> Reply {
        match request {
            Request::Status => wrap(self.status()),
            Request::Hook(report) => wrap(self.on_hook(report).await),
            Request::SessionEnd { session_id, reason } => {
                let mut reply = Reply::ok(&serde_json::json!({}));
                for shared in &self.senates {
                    let r = shared
                        .dispatch(Request::SessionEnd {
                            session_id: session_id.clone(),
                            reason: reason.clone(),
                        })
                        .await;
                    if !r.ok {
                        reply = r;
                    }
                }
                reply
            }
            Request::Presides {
                session,
                runtime,
                agent_env,
                agent_file,
            } => wrap(self.on_presides(&session, runtime, agent_env, agent_file)),
            Request::Docket { session, committee } => {
                wrap(self.on_docket(&session, committee).await)
            }
            Request::Route {
                session,
                committee,
                seq,
                to,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(seat.on_route(&speaker, committee, seq, to, id).await),
                Err(e) => Reply::err(e),
            },
            Request::Cover {
                session,
                committee,
                thread,
                body,
                agent,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(
                    seat.on_cover(&speaker, committee, thread, body, agent, id)
                        .await,
                ),
                Err(e) => Reply::err(e),
            },
            Request::Refer {
                session,
                committee,
                seq,
                reason,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => {
                    wrap(seat.on_refer(&speaker, committee, seq, reason, id).await)
                }
                Err(e) => Reply::err(e),
            },
            other => {
                let target = selector_of(&other)
                    .and_then(|selector| self.locate(selector))
                    .map(|(shared, _)| shared)
                    .unwrap_or_else(|| self.primary().clone());
                target.dispatch(other).await
            }
        }
    }

    /// Register the session in the Senate its agent selects, end it in any
    /// other it was registered in, and tell a presiding session what it
    /// presides over.
    async fn on_hook(&self, report: HookReport) -> Result<HookReply, IpcError> {
        let codex = report.runtime == Runtime::Codex;
        let (target, placed) = if self.several {
            let (rel, source) = agent::choose(
                &self.primary().config.agents,
                report.runtime,
                agent::Pins {
                    env: report.agent_env.as_deref(),
                    file: report.agent_file.as_deref(),
                },
            );
            let agent = rel.under(&self.primary().config.person);
            let Some(target) = self.senate_of(&agent) else {
                return Err(IpcError::bad_request(no_senate(
                    &agent,
                    self.primary().locale,
                )));
            };
            (target.clone(), Some((agent, source)))
        } else {
            (self.primary().clone(), None)
        };
        let event = report.event;
        let mut reply = target.on_hook(report, placed).await?;
        if codex {
            target.reconcile_approvals();
        }
        for other in &self.senates {
            if !Arc::ptr_eq(other, &target) {
                other.retire(&reply.session.session_id);
            }
        }
        if let Some(seat) = self.seat_of(&reply.session.agent)
            && let Some(text) = seat.presiding_context(&reply.session, event).await
        {
            reply.context = Some(match reply.context.take() {
                Some(context) => format!("{context}\n\n{text}"),
                None => text,
            });
        }
        Ok(reply)
    }

    /// The presiding session behind `selector`: the Senate whose seat its
    /// agent holds, and that agent. Refused for any other session.
    fn presiding(
        &self,
        selector: &SessionSelector,
    ) -> Result<(Arc<Shared>, SenatorPath), IpcError> {
        let locale = self.primary().locale;
        let Some((_, session)) = self.locate(selector) else {
            // The same refusal a session-scoped request gets.
            return Err(self.primary().resolve(selector).err().unwrap_or_else(|| {
                IpcError::new(codes::NO_SESSION, "no registered session matches")
            }));
        };
        match self.seat_of(&session.agent) {
            Some(seat) => Ok((seat.clone(), session.agent)),
            None => Err(IpcError::new(
                codes::NOT_PRESIDING,
                crate::preside::not_presiding(&session.agent, locale),
            )),
        }
    }

    fn on_presides(
        &self,
        selector: &SessionSelector,
        runtime: Runtime,
        agent_env: Option<String>,
        agent_file: Option<String>,
    ) -> Result<PresidesReply, IpcError> {
        let agent = match self.locate(selector) {
            Some((_, session)) => Some(session.agent),
            None => {
                let primary = self.primary();
                let (rel, _) = agent::choose(
                    &primary.config.agents,
                    runtime,
                    agent::Pins {
                        env: agent_env.as_deref(),
                        file: agent_file.as_deref(),
                    },
                );
                Some(if self.several {
                    rel.under(&primary.config.person)
                } else {
                    primary.seating().place(&rel)
                })
            }
        };
        let presiding = agent.as_ref().and_then(|agent| {
            let seat = self.seat_of(agent)?;
            Some(Presiding {
                speaker: agent.clone(),
                senate: seat.seating().presider().cloned(),
                seated: seat.seated(),
            })
        });
        Ok(PresidesReply { agent, presiding })
    }

    /// A presiding session's docket of the Senate it presides over; without
    /// a session, a person's read of every Senate here that one of their
    /// agents presides over.
    async fn on_docket(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
    ) -> Result<DocketReply, IpcError> {
        let named = selector.session_id.is_some() || selector.claude_pid.is_some();
        if named {
            let (seat, speaker) = self.presiding(selector)?;
            return Ok(DocketReply {
                speaker: Some(speaker),
                senates: vec![seat.docket(committee).await?],
            });
        }
        let mut senates: Vec<SenateDocket> = Vec::new();
        for shared in &self.senates {
            let mine = shared
                .speaker_seat()
                .and_then(|seat| seat.senator().cloned())
                .is_some_and(|s| s.owner() == &shared.config.person);
            if mine {
                senates.push(shared.docket(committee.clone()).await?);
            }
        }
        Ok(DocketReply {
            speaker: None,
            senates,
        })
    }
}

/// The session a request names, for the requests that name one.
fn selector_of(request: &Request) -> Option<&SessionSelector> {
    match request {
        Request::Ask { session, .. }
        | Request::Answer { session, .. }
        | Request::CatchUp { session, .. }
        | Request::Join { session, .. }
        | Request::Search { session, .. }
        | Request::Post { session, .. }
        | Request::Brief { session, .. }
        | Request::Bound { session }
        | Request::Task { session, .. }
        | Request::Escalate { session, .. }
        | Request::Take { session, .. }
        | Request::Inbox { session, .. }
        | Request::Checkpoint { session, .. } => Some(session),
        _ => None,
    }
}

/// No Senate on this machine seats `agent`.
fn no_senate(agent: &SenatorPath, locale: Locale) -> String {
    let me = Address::senator(agent).in_text();
    match locale {
        Locale::Zh => format!(
            "这台机器上没有任何 Senate 安置 {me}：它的上一级路径既不是你本人，也不是本机某个 \
             Senate 的主持者。检查 SENATE_AGENT 或 .senate/agent。"
        ),
        Locale::En => format!(
            "No Senate on this machine seats {me}: its parent path is neither you nor the \
             presider of a Senate here. Check SENATE_AGENT or .senate/agent."
        ),
    }
}

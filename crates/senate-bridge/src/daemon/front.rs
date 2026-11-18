//! The daemon's front: what the local socket talks to.
//!
//! The daemon serves one Senate ([`Shared`]): the one at `server_url`, or
//! the one `senated --machine` hosts. [`Front`] sends each request to it,
//! and handles what concerns the session holding the Speaker's seat:
//!
//! - a hook registers the session, and tells a presiding session what it
//!   presides over;
//! - a presiding request (`docket`, `route`, `cover`, `refer`, `forward`,
//!   `refuse`, and `set_chair` from a session) is served only for a
//!   session whose agent works the Speaker's desk: the Speaker itself (an
//!   agent Speaker) or one of its secretaries. It writes as `speaker` with
//!   `meta.acted_by` naming that agent.
//!
//! A presiding session sits in the Senate it presides over: its agent is a
//! senator there like any other, and is pushed what is put to the Speaker
//! as well, marked as such.

use super::*;
use crate::ipc::{DocketReply, PresidesReply, Presiding, SenateDocket};

pub(crate) struct Front {
    senates: Vec<Arc<Shared>>,
    relay: Arc<Relay>,
    watch: broadcast::Sender<WatchEvent>,
    /// Watch clients that show notifications themselves, connected now.
    app_clients: Arc<std::sync::atomic::AtomicUsize>,
}

impl Front {
    pub(crate) fn new(
        senate: Arc<Shared>,
        relay: Arc<Relay>,
        watch: broadcast::Sender<WatchEvent>,
        app_clients: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            senates: vec![senate],
            relay,
            watch,
            app_clients,
        }
    }

    /// Every Senate served: one.
    pub(crate) fn senates(&self) -> &[Arc<Shared>] {
        &self.senates
    }

    pub(crate) fn relay(&self) -> &Arc<Relay> {
        &self.relay
    }

    pub(crate) fn app_clients(&self) -> &Arc<std::sync::atomic::AtomicUsize> {
        &self.app_clients
    }

    pub(crate) fn watch(&self) -> &broadcast::Sender<WatchEvent> {
        &self.watch
    }

    /// The Senate served.
    fn primary(&self) -> &Arc<Shared> {
        &self.senates[0]
    }

    /// The Senate whose Speaker's seat `agent` holds.
    fn seat_of(&self, agent: &SenatorName) -> Option<&Arc<Shared>> {
        self.senates.iter().find(|s| s.presided_by(agent))
    }

    /// The Senate a session is registered in, and the session.
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

    /// The daemon's status.
    pub(crate) fn status(&self) -> Result<StatusReply, IpcError> {
        self.primary().on_status()
    }

    pub(crate) async fn dispatch(self: &Arc<Self>, request: Request) -> Reply {
        match request {
            Request::Status => {
                // The office, the chairs and the children as the server says
                // now, bounded: a server that does not answer leaves what
                // was last known.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    self.primary().refresh_me(),
                )
                .await;
                wrap(self.status())
            }
            Request::Hook(report) => wrap(self.on_hook(report, false).await),
            Request::SessionHook(report) => wrap(self.on_hook(report, true).await),
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
                session, runtime, ..
            } => wrap(self.on_presides(&session, runtime).await),
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
            Request::Forward {
                session,
                committee,
                seq,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(seat.on_forward(&speaker, committee, seq, id).await),
                Err(e) => Reply::err(e),
            },
            Request::Refuse {
                session,
                committee,
                seq,
                reason,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => {
                    wrap(seat.on_refuse(&speaker, committee, seq, reason, id).await)
                }
                Err(e) => Reply::err(e),
            },
            // From a session: the Speaker's office's. Without one: the
            // person's own.
            Request::SetChair {
                session,
                committee,
                senator,
            } if session.session_id.is_some() || session.claude_pid.is_some() => {
                match self.presiding(&session) {
                    Ok((seat, actor)) => wrap(
                        seat.on_set_chair(&session, committee, senator, Some(&actor))
                            .await,
                    ),
                    Err(e) => Reply::err(e),
                }
            }
            other => {
                let target = selector_of(&other)
                    .and_then(|selector| self.locate(selector))
                    .map(|(shared, _)| shared)
                    .unwrap_or_else(|| self.primary().clone());
                target.dispatch(other).await
            }
        }
    }

    /// Register the session, and tell a presiding session what it presides
    /// over.
    async fn on_hook(&self, report: HookReport, native: bool) -> Result<HookReply, IpcError> {
        let codex = report.runtime == Runtime::Codex;
        let target = self.primary().clone();
        let event = report.event;
        let mut reply = target.on_hook(report, native).await?;
        if codex {
            target.reconcile_approvals();
        }
        let agent = reply.session.agent.clone();
        let mut parts: Vec<String> = reply.context.take().into_iter().collect();
        if let Some(seat) = self.seat_of(&agent)
            && let Some(text) = seat.presiding_context(&reply.session, event).await
        {
            parts.push(text);
        }
        reply.context = (!parts.is_empty()).then(|| parts.join("\n\n"));
        Ok(reply)
    }

    /// The presiding session behind `selector`: the Senate whose seat its
    /// agent holds, and that agent. Refused for any other session.
    fn presiding(
        &self,
        selector: &SessionSelector,
    ) -> Result<(Arc<Shared>, SenatorName), IpcError> {
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

    async fn on_presides(
        &self,
        selector: &SessionSelector,
        runtime: Runtime,
    ) -> Result<PresidesReply, IpcError> {
        let primary = self.primary();
        let located = self.locate(selector).map(|(_, session)| session);
        let agent = if let Some(session) = &located {
            Some(session.agent.clone())
        } else if let Some(id) = selector.session_id.as_deref()
            && !primary.hosts.unpublished(id)
        {
            match primary.store.hosted_native_retired(id).map_err(internal)? {
                Some(false) => primary
                    .store
                    .session(id)
                    .map_err(internal)?
                    .map(|s| s.agent),
                Some(true) => None,
                None => Some(agent::session_name(runtime, id)),
            }
        } else {
            None
        };
        // The desk may have changed since the daemon last asked.
        primary.refresh_me().await;
        let presiding = agent.as_ref().and_then(|agent| {
            let seat = self.seat_of(agent)?;
            Some(Presiding {
                speaker: agent.clone(),
                role: seat.desk_role(agent).unwrap_or_default(),
                office: seat.speaker_id(),
                senate: seat.senate_name(),
                seated: seat.seated(),
            })
        });
        let mut chairs = Vec::new();
        if let Some(agent) = &agent {
            let committee = located.and_then(|session| session.committee().cloned());
            if let Some(committee) = committee
                && primary.current_chair(&committee).await.as_ref() == Some(agent)
            {
                chairs.push(committee);
            }
        }
        Ok(PresidesReply {
            agent,
            presiding,
            chairs,
        })
    }

    /// A presiding session's docket of the Senate it presides over; without
    /// a session, the person's read of it when one of their agents
    /// presides.
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
            // One of the person's agents works the desk, or the person is
            // the Speaker: the person sees the whole desk.
            let mine = shared
                .office()
                .map(|o| {
                    shared.person_is_speaker()
                        || o.agent_workers().any(|w| shared.seating().owns(w))
                })
                .unwrap_or_else(|| {
                    shared
                        .speaker_seat()
                        .and_then(|seat| seat.senator().cloned())
                        .is_some_and(|s| shared.seating().owns(&s))
                });
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
        | Request::Spawn { session, .. }
        | Request::Restore { session, .. }
        | Request::CatchUp { session, .. }
        | Request::Join { session, .. }
        | Request::JoinAs { session, .. }
        | Request::Search { session, .. }
        | Request::Post { session, .. }
        | Request::Brief { session, .. }
        | Request::Bound { session }
        | Request::Task { session, .. }
        | Request::Escalate { session, .. }
        | Request::Take { session, .. }
        | Request::Inbox { session, .. }
        | Request::Checkpoint { session, .. }
        | Request::Send { session, .. }
        | Request::Secretaries { session, .. }
        | Request::SecretaryWork { session, .. }
        | Request::ChairDocket { session }
        | Request::ChairRoute { session, .. }
        | Request::ChairCover { session, .. }
        | Request::SecretaryCover { session, .. }
        | Request::ChairEscalate { session, .. } => Some(session),
        _ => None,
    }
}

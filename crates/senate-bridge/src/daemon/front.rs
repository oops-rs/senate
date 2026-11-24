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
    pub(super) observe: Option<Arc<senate_observe::Registry>>,
    pub(super) remote: Option<Arc<crate::remote::Service>>,
    relay: Arc<Relay>,
    pub(super) process_checks: tokio::sync::Semaphore,
    #[cfg(target_os = "macos")]
    pub(super) app_confirmations: Arc<crate::app_confirmation::Queue>,
    pub(super) session_drops: tokio::sync::Semaphore,
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
            observe: None,
            remote: None,
            senates: vec![senate],
            relay,
            process_checks: tokio::sync::Semaphore::new(4),
            #[cfg(target_os = "macos")]
            app_confirmations: Arc::default(),
            session_drops: tokio::sync::Semaphore::new(2),
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
    pub(super) fn primary(&self) -> &Arc<Shared> {
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
        if crate::app_confirmation::required(&request) {
            return Reply::err(IpcError::new(
                codes::NOT_A_PERSON,
                approval_guard::APP_CONFIRMATION,
            ));
        }
        match request {
            Request::ObserveAdmin { command } => self.on_observe_admin(command).await,
            Request::SetCard {
                session,
                target,
                document,
            } if session == SessionSelector::default() => {
                wrap(self.set_observer_card(None, target, &document).await)
            }
            Request::SetCard { .. } => Reply::err(IpcError::bad_request(
                "Card writes require a verified session",
            )),
            Request::RemoteControl { .. } => Reply::err(IpcError::bad_request(
                "remote control requires the local person socket",
            )),
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
            Request::DeskItem { committee, seq } => {
                wrap(self.primary().on_desk_item(committee, seq).await)
            }
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
            Request::Hold {
                session,
                committee,
                seq,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(seat.on_hold(&speaker, committee, seq, id).await),
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
            Request::SpeakerAlias {
                session,
                committee,
                senator,
                alias,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(
                    seat.on_speaker_alias(&speaker, committee, senator, alias)
                        .await,
                ),
                Err(e) => Reply::err(e),
            },
            Request::SpeakerUnseat {
                session,
                committee,
                senator,
                id,
            } => match self.presiding(&session) {
                Ok((seat, speaker)) => wrap(
                    seat.on_speaker_unseat(&speaker, committee, senator, id)
                        .await,
                ),
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

    #[cfg(target_os = "macos")]
    pub(super) async fn dispatch_app_confirmation(&self, request: Request) -> Reply {
        match request {
            Request::RebindHost { host, to } => {
                wrap(self.primary().repair_agy_binding(&host, &to).await)
            }
            Request::ObserveAdmin {
                command: senate_observe::AdminRequest::Create { grant },
            } if grant.scope == senate_observe::Scope::Lines => {
                self.on_observe_admin(senate_observe::AdminRequest::Create { grant })
                    .await
            }
            _ => Reply::err(IpcError::bad_request("Unexpected app confirmation request")),
        }
    }

    async fn on_observe_admin(&self, command: senate_observe::AdminRequest) -> Reply {
        use senate_observe::AdminRequest;

        let Some(registry) = self.observe.clone() else {
            return Reply::err(IpcError::bad_request(
                "Observe requires a machine-hosted Senate",
            ));
        };
        let bad = |error: senate_observe::GrantError| IpcError::bad_request(error.to_string());
        let command = match command {
            AdminRequest::Preview { committee, seq } => {
                return wrap(registry.preview(&committee, seq).await.map_err(bad));
            }
            AdminRequest::Publish {
                committee,
                seq,
                reviewed,
            } => {
                return wrap(
                    registry
                        .publish_line(&committee, seq, reviewed)
                        .await
                        .map_err(bad),
                );
            }
            AdminRequest::Withdraw { committee, seq } => {
                return wrap(registry.withdraw_line(&committee, seq).await.map_err(bad));
            }
            AdminRequest::HistorySession { id } => {
                return wrap((|| -> Result<Option<String>, IpcError> {
                    let grants = registry.selector_grants(&id);
                    let sessions = self.primary().store.all_sessions().map_err(internal)?;
                    Ok(sessions
                        .into_iter()
                        .filter(|session| !session.is_live())
                        .find_map(|session| {
                            let committee = session.committee()?;
                            grants
                                .iter()
                                .any(|grant| {
                                    grant
                                        .committees
                                        .as_ref()
                                        .is_none_or(|allow| allow.contains(committee))
                                        && registry
                                            .history_id(grant, &session.session_id)
                                            .is_ok_and(|candidate| candidate == id)
                                })
                                .then_some(session.session_id)
                        }))
                })());
            }
            AdminRequest::SeatIdentity { id, committee } => {
                let grants = registry.selector_grants(&id);
                let me = match self.primary().api.me().await {
                    Ok(me) => me,
                    Err(error) => return Reply::err(server_error(error)),
                };
                let seat = me.agents.into_iter().find(|agent| {
                    agent.is_session_name()
                        && grants.iter().any(|grant| {
                            grant
                                .committees
                                .as_ref()
                                .is_none_or(|allow| allow.contains(&committee))
                                && registry
                                    .seat_id(grant, agent.as_str())
                                    .is_ok_and(|candidate| candidate == id)
                        })
                });
                return Reply::ok(&seat);
            }
            other => other,
        };
        let readable = if matches!(&command, AdminRequest::Create { .. }) {
            match self.primary().api.me().await {
                Ok(me) => me.committees.into_iter().chain(me.archived).collect(),
                Err(error) => return Reply::err(server_error(error)),
            }
        } else {
            BTreeSet::new()
        };
        match tokio::task::spawn_blocking(move || match command {
            AdminRequest::Create { grant } => wrap(
                registry
                    .create(grant, readable)
                    .map_err(|error| IpcError::bad_request(error.to_string())),
            ),
            AdminRequest::List => Reply::ok(&registry.list()),
            AdminRequest::SeatIdentity { .. }
            | AdminRequest::HistorySession { .. }
            | AdminRequest::Preview { .. }
            | AdminRequest::Publish { .. }
            | AdminRequest::Withdraw { .. } => {
                Reply::err(IpcError::internal("unexpected Observe preview request"))
            }
            AdminRequest::Identity => wrap(
                registry
                    .installation()
                    .map_err(|error| IpcError::bad_request(error.to_string())),
            ),
            AdminRequest::Revoke { id } => wrap(
                registry
                    .revoke(&id)
                    .map_err(|error| IpcError::bad_request(error.to_string())),
            ),
        })
        .await
        {
            Ok(reply) => reply,
            Err(error) => Reply::err(IpcError::internal(error)),
        }
    }

    /// Register the session, and tell a presiding session what it presides over.
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
            None => {
                let desk: Vec<SenatorName> = self
                    .senates
                    .iter()
                    .filter_map(|s| s.office())
                    .flat_map(|office| office.agent_workers().cloned().collect::<Vec<_>>())
                    .collect();
                Err(IpcError::new(
                    codes::NOT_PRESIDING,
                    crate::preside::not_presiding(&session.agent, &desk, locale),
                ))
            }
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
        | Request::Stop { session, .. }
        | Request::CatchUp { session, .. }
        | Request::Join { session, .. }
        | Request::JoinAs { session, .. }
        | Request::Search { session, .. }
        | Request::Memory { session, .. }
        | Request::MemoryEdit { session, .. }
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

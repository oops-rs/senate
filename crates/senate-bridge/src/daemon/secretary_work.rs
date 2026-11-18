use senate_core::api::Secretaries;
use senate_core::secretary_work::{
    SecretaryReminder, SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkRequest,
    SetSecretarySession,
};

use super::*;

pub(super) const REMINDER_POLL: Duration = Duration::from_secs(2);
const REMINDER_API_TIMEOUT: Duration = Duration::from_millis(800);
pub(super) const HOOK_REMINDER_TIMEOUT: Duration = Duration::from_millis(250);

pub(super) fn reminder_text(l: Locale, reminder: &SecretaryReminder) -> String {
    let team = &reminder.principal;
    let item = &reminder.item;
    l.pick(
        format!("[senate · 秘书团队] {team} 的事项 {item} 已指派给本会话。用 mcp__senate__secretary_work 的 read 查看。"),
        format!("[senate · secretary team] Item {item} for {team} is assigned to this session. Read it with mcp__senate__secretary_work."),
    )
}

fn native_session(session: &Session) -> bool {
    session.agent_source == agent::AgentSource::Session
        && !Shared::is_companion(session)
        && session.agent == agent::session_name(session.runtime, &session.session_id)
}

impl Shared {
    fn person_senator(&self) -> Result<SenatorName, IpcError> {
        SenatorName::new(self.config.person.clone())
            .map_err(|e| IpcError::bad_request(e.to_string()))
    }

    pub(super) async fn on_set_secretary_session(
        &self,
        target: Option<String>,
        senator: Option<SenatorName>,
        principal: Option<SenatorName>,
        appointed: bool,
        acted_by: Option<SenatorName>,
    ) -> Result<Secretaries, IpcError> {
        let senator = match (target, senator) {
            (Some(id), None) => self
                .store
                .session(&id)
                .map_err(internal)?
                .filter(native_session)
                .map(|session| session.agent)
                .ok_or_else(|| {
                    IpcError::bad_request("name the full ID of a registered coding session")
                })?,
            (None, Some(senator)) if !appointed => senator,
            _ => {
                return Err(IpcError::bad_request(
                    "use a session ID to appoint, or a session ID or registered senator to dismiss",
                ));
            }
        };
        let principal = principal.unwrap_or(self.person_senator()?);
        let result = self
            .api
            .set_secretary_session(
                &principal,
                &SetSecretarySession {
                    senator,
                    appointed,
                    acted_by,
                },
            )
            .await
            .map_err(server_error)?;
        self.refresh_me().await;
        Ok(result)
    }

    pub(super) async fn on_secretary_work(
        &self,
        selector: &SessionSelector,
        principal: Option<SenatorName>,
        operation: SecretaryWorkOp,
    ) -> Result<SecretaryWorkReply, IpcError> {
        let (actor, principal) = if selector.session_id.is_none() && selector.claude_pid.is_none() {
            let actor = self.person_senator()?;
            (actor.clone(), principal.unwrap_or(actor))
        } else {
            let session = self.resolve(selector)?;
            if !native_session(&session) {
                return Err(IpcError::bad_request(
                    "team work needs a registered native coding session",
                ));
            }
            let principal = match principal {
                Some(principal) => principal,
                None if self
                    .office()
                    .is_some_and(|office| office.speaker_here() == Some(&session.agent)) =>
                {
                    session.agent.clone()
                }
                None => {
                    let all = self.api.secretaries().await.map_err(server_error)?;
                    let mut teams = all.into_iter().filter_map(|(team, members)| {
                        members.contains(&session.agent).then_some(team)
                    });
                    match (teams.next(), teams.next()) {
                        (Some(team), None) => team,
                        (None, _) => {
                            return Err(IpcError::bad_request(
                                "this session has no secretary team",
                            ));
                        }
                        _ => {
                            return Err(IpcError::bad_request(
                                "name a principal when this session belongs to several teams",
                            ));
                        }
                    }
                }
            };
            (session.agent, principal)
        };
        self.api
            .secretary_work(&principal, &SecretaryWorkRequest { actor, operation })
            .await
            .map_err(server_error)
    }

    /// A Claude session using only its hooks receives a pending assignment
    /// on its next hook, even when it has no committee binding.
    pub(super) async fn secretary_hook_reminder(
        &self,
        session: &Session,
    ) -> Option<SecretaryReminder> {
        if session.runtime != Runtime::Claude
            || self.ladder.can_push(session)
            || !native_session(session)
        {
            return None;
        }
        let reminders = tokio::time::timeout(
            HOOK_REMINDER_TIMEOUT,
            self.api
                .secretary_reminders(std::slice::from_ref(&session.agent)),
        )
        .await
        .ok()?
        .ok()?;
        reminders
            .into_iter()
            .find(|reminder| reminder.to == session.agent)
    }

    /// Private reminders use the same sequential delivery loop and spacing
    /// as the committee inbox. The server retains each pending assignment
    /// until a transport accepts it and the bridge acknowledges it.
    pub(super) async fn push_secretary_reminders(
        &self,
        sent: &mut BTreeSet<SecretaryReminder>,
    ) -> Result<(), StoreError> {
        let sessions: BTreeMap<_, _> = self
            .store
            .live_sessions()?
            .into_iter()
            .filter(|s| native_session(s) && self.ladder.can_push(s))
            .map(|s| (s.agent.clone(), s.session_id))
            .collect();
        if sessions.is_empty() {
            return Ok(());
        }
        let agents: Vec<_> = sessions.keys().cloned().collect();
        let Ok(Ok(reminders)) =
            tokio::time::timeout(REMINDER_API_TIMEOUT, self.api.secretary_reminders(&agents)).await
        else {
            return Ok(());
        };
        sent.retain(|item| reminders.contains(item));
        for reminder in reminders {
            if !sent.contains(&reminder) {
                let Some(sid) = sessions.get(&reminder.to) else {
                    continue;
                };
                let _binding = self.session_bindings.read().await;
                let Some(session) = self.store.session(sid)?.filter(|s| {
                    s.agent == reminder.to
                        && s.is_live()
                        && native_session(s)
                        && self.ladder.can_push(s)
                }) else {
                    continue;
                };
                if !matches!(self.hold(sid, false, Instant::now()), Hold::Ready) {
                    continue;
                }
                let text = reminder_text(self.locale, &reminder);
                let batch = Batch {
                    committee: None,
                    events: Vec::new(),
                    text,
                };
                self.note_attempt(sid);
                if self.ladder.deliver(&session, &batch).await.outcome != DeliveryOutcome::Injected
                {
                    continue;
                }
                sent.insert(reminder.clone());
            }
            let _ = tokio::time::timeout(
                REMINDER_API_TIMEOUT,
                self.api.ack_secretary_reminder(&reminder),
            )
            .await;
        }
        Ok(())
    }
}

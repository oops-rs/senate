//! Daemon authorization, capacity, and audit for Senate-hosted sessions.

use senate_core::SenatorKind;

use super::*;
use crate::ipc::{HostedRuntime, SpawnReply};

const ROLE_TIMEOUT: Duration = Duration::from_secs(4);
const AUDIT_TIMEOUT: Duration = Duration::from_secs(4);

struct HostAuthority {
    sender: Address,
    acted_by: Option<SenatorName>,
    committee_scope: Option<CommitteeId>,
    may_appoint: bool,
    committees: Vec<CommitteeId>,
}

impl HostAuthority {
    fn check_target(&self, committee: &CommitteeId, appoint_chair: bool) -> Result<(), IpcError> {
        if (appoint_chair && !self.may_appoint)
            || self
                .committee_scope
                .as_ref()
                .is_some_and(|allowed| allowed != committee)
        {
            return Err(IpcError::new(
                codes::NOT_PRESIDING,
                "only the Speaker's office may start a chair or start or restore a senator in another committee",
            ));
        }
        if !self.committees.contains(committee) {
            return Err(IpcError::bad_request(format!(
                "unknown committee {committee}"
            )));
        }
        Ok(())
    }
}

fn not_hosting() -> IpcError {
    IpcError::new(
        codes::NOT_CHAIRING,
        "only the Speaker's office or this committee's chair may start or restore a senator",
    )
}

impl Shared {
    async fn authorize_host_launch(
        &self,
        selector: &SessionSelector,
    ) -> Result<HostAuthority, IpcError> {
        let me = tokio::time::timeout(ROLE_TIMEOUT, self.api.me())
            .await
            .map_err(|_| IpcError::new(codes::SERVER, "could not verify Senate roles in time"))?
            .map_err(server_error)?;
        // The empty selector is the person's CLI. The MCP shim must supply
        // its session identity, including when asking to restore a host.
        if selector.session_id.is_none() && selector.claude_pid.is_none() {
            let speaker = me.office.is_some_and(|office| {
                office.speaker.name().name() == &me.person
                    && !matches!(office.kind, SenatorKind::Agent | SenatorKind::BuiltIn)
            });
            return Ok(HostAuthority {
                sender: Address::senior(&me.person),
                acted_by: None,
                committee_scope: None,
                may_appoint: speaker,
                committees: me.committees,
            });
        }
        let session = self.resolve(selector)?;
        if !session.is_live() {
            return Err(IpcError::new(
                codes::NO_SESSION,
                "the calling session is not live",
            ));
        }
        if me.office.is_some_and(|office| {
            office
                .agent_workers()
                .any(|worker| worker == &session.agent)
        }) {
            return Ok(HostAuthority {
                sender: Address::Speaker,
                acted_by: Some(session.agent),
                committee_scope: None,
                may_appoint: true,
                committees: me.committees,
            });
        }
        let committee = session.committee().ok_or_else(not_hosting)?;
        let chair = tokio::time::timeout(ROLE_TIMEOUT, self.api.chair(committee))
            .await
            .map_err(|_| {
                IpcError::new(
                    codes::SERVER,
                    "could not verify the committee chair in time",
                )
            })?
            .map_err(server_error)?;
        if chair.chair.as_ref() != Some(&session.agent) {
            return Err(not_hosting());
        }
        Ok(HostAuthority {
            sender: Address::senator(&session.agent),
            acted_by: None,
            committee_scope: Some(committee.clone()),
            may_appoint: false,
            committees: me.committees,
        })
    }

    pub(super) async fn on_hosted_spawn(
        &self,
        selector: &SessionSelector,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        appoint_chair: bool,
    ) -> Result<SpawnReply, IpcError> {
        let auth = self.authorize_host_launch(selector).await?;
        auth.check_target(&committee, appoint_chair)?;
        let launch = self
            .on_spawn(
                runtime,
                committee,
                alias,
                cwd,
                Some(self.config.hosted_session_limit),
            )
            .await?;
        self.finish_hosted_launch(&auth, false, appoint_chair, launch)
            .await
    }

    pub(super) async fn on_hosted_restore(
        &self,
        selector: &SessionSelector,
        senator: &SenatorName,
    ) -> Result<SpawnReply, IpcError> {
        let auth = self.authorize_host_launch(selector).await?;
        let target = self.resolve_restore(senator).await?;
        auth.check_target(&target.committee, false)?;
        self.check_restore_seat(&target.committee, &target.session.agent)
            .await?;
        let launch = self
            .on_restore(target, Some(self.config.hosted_session_limit))
            .await?;
        self.finish_hosted_launch(&auth, true, false, launch).await
    }

    async fn check_restore_seat(
        &self,
        committee: &CommitteeId,
        senator: &SenatorName,
    ) -> Result<(), IpcError> {
        let status = tokio::time::timeout(
            ROLE_TIMEOUT,
            self.api.committee_seat_status(committee, senator),
        )
        .await
        .map_err(|_| {
            IpcError::new(
                codes::SERVER,
                self.locale.pick(
                    "查询委员会席位时服务器超时。",
                    "The server timed out checking the committee seat.",
                ),
            )
        })?
        .map_err(server_error)?;
        if status.unseated {
            let who = Address::senator(senator).in_text();
            let join = format!("senate seat join --committee {committee} --senator {senator}");
            let message = match self.locale {
                Locale::En => format!(
                    "{who} was unseated from committee `{committee}`. Run `{join}` before restoring."
                ),
                Locale::Zh => {
                    format!("{who} 已被委员会 `{committee}` 除席。请先运行 `{join}`，再恢复会话。")
                }
            };
            return Err(IpcError::bad_request(message));
        }
        Ok(())
    }

    async fn finish_hosted_launch(
        &self,
        auth: &HostAuthority,
        restoring: bool,
        appoint_chair: bool,
        launch: hosts::HostedLaunch,
    ) -> Result<SpawnReply, IpcError> {
        if let Err(error) = self.audit_host_launch(auth, restoring, &launch.reply).await {
            if let Err(stop_error) = launch.process.stop().await {
                return Err(IpcError::new(
                    &error.code,
                    format!(
                        "{error}. Could not stop the unaudited host for {}: {stop_error}",
                        Address::senator(&launch.reply.senator).in_text()
                    ),
                ));
            }
            return Err(error);
        }
        let reply = launch.finish();
        if appoint_chair {
            self.appoint_hosted_chair(auth, &reply).await?;
        }
        Ok(reply)
    }

    /// Finish the launch before returning its successful IPC reply.
    async fn audit_host_launch(
        &self,
        auth: &HostAuthority,
        restoring: bool,
        reply: &SpawnReply,
    ) -> Result<(), IpcError> {
        let committee = &reply.committee;
        let target = Address::senator(&reply.senator).in_text();
        let actor = auth.acted_by.as_ref().map_or_else(
            || auth.sender.in_text(),
            |agent| Address::senator(agent).in_text(),
        );
        let runtime = self
            .store
            .session(&reply.session_id)
            .map_err(internal)?
            .ok_or_else(|| {
                IpcError::new(
                    codes::NO_SESSION,
                    "host registration disappeared before audit",
                )
            })?
            .runtime
            .as_str();
        let verb = match (self.locale, restoring) {
            (Locale::En, false) => "spawned",
            (Locale::En, true) => "restored",
            (Locale::Zh, false) => "启动了",
            (Locale::Zh, true) => "恢复了",
        };
        let body = match self.locale {
            Locale::Zh => format!("{actor} 在委员会 `{committee}` {verb} {target}（{runtime}）。"),
            Locale::En => {
                format!("{actor} {verb} {target} ({runtime}) in the committee `{committee}`.")
            }
        };
        let mut event = NewEvent::new(committee.clone(), Kind::Note, auth.sender.clone(), body);
        if let Some(actor) = &auth.acted_by {
            event = event.with_meta(meta::ACTED_BY, serde_json::json!(actor));
        }
        tokio::time::timeout(AUDIT_TIMEOUT, self.post(event))
            .await
            .map_err(|_| {
                IpcError::new(
                    codes::SERVER,
                    "the spawn or restore audit was not acknowledged",
                )
            })??;
        Ok(())
    }

    /// After the launch is audited, appoint the chair on the spawner's
    /// authority. A missing HTTP acknowledgement may hide a successful
    /// appointment, so the caller keeps this registered host running.
    async fn appoint_hosted_chair(
        &self,
        auth: &HostAuthority,
        reply: &SpawnReply,
    ) -> Result<(), IpcError> {
        let timeout = IpcError::new(codes::SERVER, "the chair appointment was not acknowledged");
        let result = tokio::time::timeout(AUDIT_TIMEOUT, async {
            // The local hook can register before the stream seats the agent.
            self.heartbeat(&reply.senator, &reply.committee).await;
            self.on_set_chair(
                &SessionSelector::default(),
                Some(reply.committee.clone()),
                reply.senator.clone(),
                auth.acted_by.as_ref(),
            )
            .await
        })
        .await
        .unwrap_or(Err(timeout));
        result.map(|_| ()).map_err(|error| {
            IpcError::new(
                &error.code,
                format!(
                    "{} was started and recorded in `{}`. Chair appointment could not be confirmed. {}",
                    Address::senator(&reply.senator).in_text(),
                    reply.committee,
                    error.message
                ),
            )
        })
    }
}

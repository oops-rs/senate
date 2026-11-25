//! Daemon authorization, capacity, and audit for Senate-hosted sessions.

use senate_core::SenatorKind;

use super::hosts::LaunchAudit;
use super::spawn_attempt::SpawnAttempt;
use super::*;
use crate::ipc::{HostedModel, HostedRuntime, SpawnProfile, SpawnReply, StopReply};

const ROLE_TIMEOUT: Duration = Duration::from_secs(4);
const AUDIT_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) struct HostAuthority {
    pub(super) sender: Address,
    pub(super) acted_by: Option<SenatorName>,
    committee_scope: Option<CommitteeId>,
    may_appoint: bool,
    committees: Vec<CommitteeId>,
    /// The archived committees: refused by name, with how to unarchive.
    archived: Vec<CommitteeId>,
    desk: Option<SenatorName>,
    locale: Locale,
}

/// Why a session cannot be started on `committee`, which nothing has been
/// written to yet, and how the committee comes to exist: spawn never
/// creates one, `senate join` by a person does.
pub(super) fn unknown_committee(committee: &CommitteeId, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!(
            "unknown committee {committee}：还没有人往这个委员会写过东西，spawn 不会创建委员会。\
             先由人在它的项目目录里运行 `senate join {committee}` 把它开出来，再 spawn。"
        ),
        Locale::En => format!(
            "unknown committee {committee}: nothing has been written to it yet, and spawn does \
             not create a committee. A person opens it first with `senate join {committee}` in \
             its project directory; then spawn again."
        ),
    }
}

impl HostAuthority {
    pub(super) fn check_target(
        &self,
        committee: &CommitteeId,
        appoint_role: bool,
    ) -> Result<(), IpcError> {
        if (appoint_role && !self.may_appoint)
            || self
                .committee_scope
                .as_ref()
                .is_some_and(|allowed| allowed != committee)
        {
            return Err(IpcError::new(
                codes::NOT_PRESIDING,
                "only the Speaker's office may start a chair or secretary, or manage a hosted senator in another committee",
            ));
        }
        if self.archived.contains(committee) {
            return Err(IpcError::bad_request(super::archived_refusal(
                committee,
                self.locale,
            )));
        }
        if !self.committees.contains(committee) {
            return Err(IpcError::bad_request(unknown_committee(
                committee,
                self.locale,
            )));
        }
        Ok(())
    }

    pub(super) fn actor(&self) -> String {
        self.acted_by.as_ref().map_or_else(
            || self.sender.in_text(),
            |agent| Address::senator(agent).in_text(),
        )
    }

    pub(super) fn note(&self, committee: CommitteeId, body: String) -> NewEvent {
        self.event(committee, Kind::Note, body)
    }

    /// An event of `kind` from whoever acts, naming the secretary who acts
    /// for the Speaker's office.
    pub(super) fn event(&self, committee: CommitteeId, kind: Kind, body: String) -> NewEvent {
        let event = NewEvent::new(committee, kind, self.sender.clone(), body);
        match &self.acted_by {
            Some(actor) => event.with_meta(meta::ACTED_BY, serde_json::json!(actor)),
            None => event,
        }
    }
}

/// The caller's spawn arguments before applying a machine-local profile.
#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct SpawnInput {
    pub(super) runtime: Option<HostedRuntime>,
    pub(super) profile: Option<SpawnProfile>,
    pub(super) committee: CommitteeId,
    pub(super) alias: Option<SenatorName>,
    pub(super) cwd: PathBuf,
    pub(super) chair: bool,
    pub(super) secretary: bool,
    pub(super) prompt: Option<String>,
    pub(super) model: HostedModel,
}

/// What a hosted spawn asks for ([`Request::Spawn`](crate::ipc::Request::Spawn)).
#[derive(Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(super) struct SpawnOrder {
    pub(super) runtime: HostedRuntime,
    pub(super) committee: CommitteeId,
    pub(super) alias: Option<SenatorName>,
    pub(super) default_alias: bool,
    pub(super) cwd: PathBuf,
    /// Appoint the new senator as the committee's chair.
    pub(super) chair: bool,
    /// Appoint the new senator to the Speaker’s desk.
    pub(super) secretary: bool,
    /// The new seat's first request, if any.
    pub(super) prompt: Option<String>,
    /// The runtime's model and effort; unset, its own default.
    pub(super) model: HostedModel,
}

fn not_hosting() -> IpcError {
    IpcError::new(
        codes::NOT_CHAIRING,
        "only the Speaker's office or this committee's chair may start, restore, or stop a hosted senator",
    )
}

impl Shared {
    pub(super) async fn authorize_host_action(
        &self,
        selector: &SessionSelector,
    ) -> Result<HostAuthority, IpcError> {
        let me = tokio::time::timeout(ROLE_TIMEOUT, self.api.me())
            .await
            .map_err(|_| IpcError::new(codes::SERVER, "could not verify Senate roles in time"))?
            .map_err(server_error)?;
        let desk = me.office.as_ref().and_then(|o| o.desk_owner()).cloned();
        // The empty selector is the person's CLI. The MCP shim must supply
        // its session identity for every hosted-session action.
        if selector.session_id.is_none() && selector.claude_pid.is_none() {
            let speaker = me.office.as_ref().is_some_and(|office| {
                office.speaker.name().name() == &me.person
                    && !matches!(office.kind, SenatorKind::Agent | SenatorKind::BuiltIn)
            });
            return Ok(HostAuthority {
                sender: Address::senior(&me.person),
                acted_by: None,
                committee_scope: None,
                may_appoint: speaker,
                desk,
                committees: me.committees,
                archived: me.archived,
                locale: self.locale,
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
                desk,
                committees: me.committees,
                archived: me.archived,
                locale: self.locale,
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
            desk: None,
            committees: me.committees,
            archived: me.archived,
            locale: self.locale,
        })
    }

    pub(super) fn resolve_spawn_order(&self, input: SpawnInput) -> Result<SpawnOrder, IpcError> {
        let (runtime, alias, model) =
            self.spawn_settings(input.runtime, input.profile, input.alias, input.model)?;
        Ok(SpawnOrder {
            runtime,
            committee: input.committee,
            alias,
            default_alias: true,
            cwd: input.cwd,
            chair: input.chair,
            secretary: input.secretary,
            prompt: input.prompt,
            model,
        })
    }

    /// The launch profiles as the configuration file has them now, so an
    /// added or edited profile needs no restart and `senate profiles`
    /// (which reads the file) and the daemon never disagree. A file that
    /// no longer parses is an error naming what is wrong, not the old
    /// profiles; with no file at all (a daemon configured in memory) they
    /// are the ones it started with.
    pub(super) fn launch_profiles(
        &self,
    ) -> Result<BTreeMap<String, crate::config::LaunchProfile>, IpcError> {
        match crate::config::BridgeConfig::load_unchecked(&Paths::new(&self.home).config()) {
            Ok(config) => Ok(config.profiles),
            Err(crate::config::ConfigError::Missing(_)) => Ok(self.config.profiles.clone()),
            Err(error) => Err(IpcError::bad_request(format!(
                "cannot read the launch profiles: {error}"
            ))),
        }
    }

    fn spawn_settings(
        &self,
        runtime: Option<HostedRuntime>,
        profile: Option<SpawnProfile>,
        alias: Option<SenatorName>,
        model: HostedModel,
    ) -> Result<(HostedRuntime, Option<SenatorName>, HostedModel), IpcError> {
        let Some(SpawnProfile {
            name,
            runtime: asserted_runtime,
        }) = profile
        else {
            return runtime
                .map(|runtime| (runtime, alias, model))
                .ok_or_else(|| IpcError::bad_request("specify a runtime or launch profile"));
        };
        let profiles = self.launch_profiles()?;
        let profile = profiles.get(&name).ok_or_else(|| {
            IpcError::bad_request(format!(
                "unknown launch profile {name:?}; see senate profiles"
            ))
        })?;
        if let Some(runtime) = [runtime, asserted_runtime]
            .into_iter()
            .flatten()
            .find(|runtime| *runtime != profile.runtime)
        {
            return Err(IpcError::bad_request(format!(
                "runtime {} conflicts with launch profile {name:?}, which uses {}",
                Runtime::from(runtime).as_str(),
                Runtime::from(profile.runtime).as_str()
            )));
        }
        let alias = match alias {
            Some(alias) => alias,
            None => name.parse().map_err(|error| {
                IpcError::bad_request(format!(
                    "launch profile {name:?} cannot be used as a default alias: {error}; supply an alias"
                ))
            })?,
        };
        let model = HostedModel::new(model.model, model.effort, model.speed);
        Ok((
            profile.runtime,
            Some(alias),
            profile.model.overridden_by(&model),
        ))
    }

    pub(super) async fn on_hosted_spawn(
        &self,
        selector: &SessionSelector,
        order: SpawnOrder,
    ) -> Result<SpawnReply, IpcError> {
        let auth = self.authorize_host_action(selector).await?;
        self.spawn_authorized(&auth, order, None).await
    }

    pub(super) async fn spawn_authorized(
        &self,
        auth: &HostAuthority,
        order: SpawnOrder,
        mut attempt: Option<&mut SpawnAttempt>,
    ) -> Result<SpawnReply, IpcError> {
        let SpawnOrder {
            runtime,
            committee,
            alias,
            default_alias,
            cwd,
            chair: appoint_chair,
            secretary: appoint_secretary,
            prompt,
            model,
        } = order;
        // Nothing starts for an effort the runtime would not take.
        let model = HostedModel::new(model.model, model.effort, model.speed);
        model.check(runtime).map_err(IpcError::bad_request)?;
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        if appoint_chair && appoint_secretary {
            return Err(IpcError::bad_request("choose either chair or secretary"));
        }
        auth.check_target(&committee, appoint_chair || appoint_secretary)?;
        let desk = appoint_secretary
            .then(|| {
                auth.desk.as_ref().ok_or_else(|| {
                    let speaker = self.office().map(|o| o.speaker);
                    IpcError::bad_request(super::roles::roles_text::desk_elsewhere(
                        speaker.as_ref(),
                        self.locale,
                    ))
                })
            })
            .transpose()?;
        let alias_guard = if default_alias {
            Some(self.spawn_aliases.lock().await)
        } else {
            None
        };
        let alias = if alias.is_none() && default_alias {
            Some(self.next_spawn_alias(runtime.into(), &committee).await?)
        } else {
            alias
        };
        if let Some(alias) = &alias {
            // Refused here before anything starts. An alias claimed between
            // this check and publication still fails at the final alias
            // step, which unwinds the staged session; no test reaches that
            // late path any more (the restart test now stops here), so it
            // wants one that has the server claim the alias mid-launch.
            self.check_alias_free(&committee, alias).await?;
        }
        let audit_auth = auth;
        let launch = self
            .on_spawn_with(
                runtime,
                committee,
                alias,
                cwd,
                model,
                self.config.hosted_session_limit,
                attempt.as_deref_mut(),
                move |phase, reply, runtime| async move {
                    self.audit_host_launch(audit_auth, false, phase, reply, runtime)
                        .await
                },
            )
            .await?;
        let mut reply = launch.finish();
        if let Some(attempt) = attempt.as_deref_mut() {
            attempt.record_created(&self.store, &reply)?;
        }
        drop(alias_guard);
        if reply.display_name.is_some() {
            *lock(&self.display_aliases) = None;
            self.refresh_display_aliases().await;
            if let Some((_, aliases)) = lock(&self.display_aliases).as_ref() {
                reply.display_name = Some(senate_core::alias::display_name(
                    Some(aliases),
                    Some(&reply.committee),
                    &reply.senator,
                ));
            }
        }
        if appoint_chair {
            self.appoint_hosted_chair(auth, &reply).await?;
        }
        if let Some(desk) = desk {
            self.appoint_hosted_secretary(auth, &reply, desk).await?;
        }
        if let Some(prompt) = prompt {
            // The seat is published: an ordinary request reaches it through
            // its bridge like any other, held for triage as any other.
            let event = auth
                .event(reply.committee.clone(), Kind::Request, prompt)
                .to(Address::senator(&reply.senator));
            let posted = tokio::time::timeout(AUDIT_TIMEOUT, self.post(event))
                .await
                .map_err(|_| {
                    IpcError::new(
                        codes::SERVER,
                        format!(
                            "{} started, but its prompt was not acknowledged",
                            Address::senator(&reply.senator).in_text()
                        ),
                    )
                })?
                .map_err(|e| {
                    IpcError::new(
                        &e.code,
                        format!(
                            "{} started, but its prompt was not sent: {}",
                            Address::senator(&reply.senator).in_text(),
                            e.message
                        ),
                    )
                })?;
            reply.request = Some(posted.seq);
        }
        if let Some(attempt) = attempt {
            attempt.record_created(&self.store, &reply)?;
        }
        Ok(reply)
    }

    pub(super) async fn on_hosted_restore(
        &self,
        selector: &SessionSelector,
        senator: &AliasName,
        model: HostedModel,
    ) -> Result<SpawnReply, IpcError> {
        let auth = self.authorize_host_action(selector).await?;
        let target = self
            .resolve_restore(senator, auth.committee_scope.as_ref())
            .await?;
        // The record, with what this restore changes of it.
        let model = HostedModel::new(model.model, model.effort, model.speed);
        target
            .model
            .overridden_by(&model)
            .check(target.runtime)
            .map_err(IpcError::bad_request)?;
        auth.check_target(&target.committee, false)?;
        self.check_restore_seat(&target.committee, &target.session.agent)
            .await?;
        let launch_senator = target.session.agent.clone();
        // So a refusal can name the session by its alias.
        self.refresh_display_aliases().await;
        let audit_auth = &auth;
        let launch = self
            .on_restore_with(
                target,
                model,
                self.config.hosted_session_limit,
                move |phase, reply, runtime| async move {
                    self.audit_host_launch(audit_auth, true, phase, reply, runtime)
                        .await
                },
            )
            .await?;
        // Restored: no longer paused nor stopped, however it was brought back.
        if let Err(error) = self.store.clear_paused(&launch_senator) {
            tracing::warn!(%error, senator = %launch_senator, "clearing a restored pause");
        }
        if let Err(error) = self.store.clear_stopped(&launch_senator) {
            tracing::warn!(%error, senator = %launch_senator, "clearing a restored stop");
        }
        Ok(launch.finish())
    }

    async fn check_restore_seat(
        &self,
        committee: &CommitteeId,
        senator: &SenatorName,
    ) -> Result<(), IpcError> {
        if self.seat_is_unseated(committee, senator).await? {
            let who = Address::senator(senator).in_text();
            let join = format!("senate senator seat {senator} --committee {committee}");
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

    pub(super) async fn seat_is_unseated(
        &self,
        committee: &CommitteeId,
        senator: &SenatorName,
    ) -> Result<bool, IpcError> {
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
        Ok(status.unseated)
    }

    pub(super) async fn on_hosted_stop(
        &self,
        selector: &SessionSelector,
        senator: &AliasName,
        force: bool,
    ) -> Result<StopReply, IpcError> {
        if force && (selector.session_id.is_some() || selector.claude_pid.is_some()) {
            return Err(IpcError::bad_request("only the local CLI may force a stop"));
        }
        let auth = self.authorize_host_action(selector).await?;
        let target = self
            .resolve_stop(senator, auth.committee_scope.as_ref())
            .await?;
        auth.check_target(&target.committee, false)?;
        let primary = &target.senator;
        if auth.committee_scope.is_some() {
            if auth.sender.as_senator() == Some(primary) {
                return Err(IpcError::bad_request(format!(
                    "a chair cannot stop its own terminal through this tool. Ask your operator to run senate session stop {primary} or exit the terminal",
                )));
            }
            if self.seat_is_unseated(&target.committee, primary).await? {
                return Err(IpcError::new(
                    codes::NOT_CHAIRING,
                    "that senator is no longer a member of your committee",
                ));
            }
        }
        // Stopped on purpose: not to be resumed, nor started again unasked.
        self.store.clear_paused(primary).map_err(internal)?;
        let reply = self
            .stop_audited(&auth, &target, force, false, false)
            .await?;
        self.store
            .mark_stopped(primary, chrono::Utc::now())
            .map_err(internal)?;
        Ok(reply)
    }

    /// Stop `target`'s terminal and leave the audit note for it: `paused`,
    /// that it was paused rather than stopped.
    pub(super) async fn stop_audited(
        &self,
        auth: &HostAuthority,
        target: &super::hosts::StopTarget,
        force: bool,
        paused: bool,
        idle_only: bool,
    ) -> Result<StopReply, IpcError> {
        let primary = &target.senator;
        let actor = auth.actor();
        let who = Address::senator(primary).in_text();
        let committee = &target.committee;
        let mut body = if paused {
            self.locale.pick(
                format!("{actor} 暂停了委员会 `{committee}` 中 {who} 的托管终端，稍后恢复。"),
                format!(
                    "{actor} paused the hosted terminal for {who} in committee `{committee}`, to be resumed."
                ),
            )
        } else {
            self.locale.pick(
                format!("{actor} 停止了委员会 `{committee}` 中 {who} 的托管终端。"),
                format!(
                    "{actor} stopped the hosted terminal for {who} in committee `{committee}`."
                ),
            )
        };
        if force {
            body.push_str(
                self.locale
                    .pick(" 此操作为强制停止。", " This was a forced stop."),
            );
        }
        let event = auth.note(committee.clone(), body);
        let mut unconfirmed = event.clone();
        let event = event.with_meta(
            super::activity::LIFECYCLE,
            serde_json::json!([if paused { "paused" } else { "stopped" }, primary]),
        );
        let verb = if force {
            self.locale.pick("强制停止", "forced stop")
        } else {
            self.locale.pick("停止", "stop")
        };
        unconfirmed.body = self.locale.pick(
            format!("{actor} 请求{verb}委员会 `{committee}` 中 {who} 的托管终端。终端主机已退出，停止结果未获确认。"),
            format!("{actor} requested a {verb} of the hosted terminal for {who} in committee `{committee}`. The host ended with the stop result unconfirmed."),
        );
        let _audit = self.track_stop_notice(event.id.clone(), primary);
        let (session_id, recorded) = self
            .stop_host(target, force, unconfirmed, idle_only)
            .await?;
        if !recorded {
            return Err(IpcError::internal(format!(
                "the terminal for {who} stopped, but the daemon did not acknowledge its session end",
            )));
        }
        let audit_pending = !matches!(
            tokio::time::timeout(AUDIT_TIMEOUT, self.post(event.clone())).await,
            Ok(Ok(_))
        );
        if audit_pending {
            self.store.publish_stop_notice(&event.id, Some(&event)).map_err(|error| {
                IpcError::internal(format!(
                    "the terminal for {who} stopped, but its log note could not be queued: {error}",
                ))
            })?;
        } else if let Err(error) = self.store.remove_hosted_switch_notice(&event.id) {
            tracing::warn!(%error, id = %event.id, "clearing a posted terminal stop notice");
        }
        Ok(StopReply {
            senator: primary.clone(),
            session_id,
            committee: committee.clone(),
            audit_pending,
        })
    }

    /// Attribute the ready host to its authorized caller before publication.
    /// A publication failure after this audit gets a matching rollback note.
    async fn audit_host_launch(
        &self,
        auth: &HostAuthority,
        restoring: bool,
        phase: LaunchAudit,
        reply: SpawnReply,
        runtime: HostedRuntime,
    ) -> Result<(), IpcError> {
        let committee = &reply.committee;
        let target = Address::senator(&reply.senator).in_text();
        let actor = auth.actor();
        let runtime = Runtime::from(runtime).as_str();
        let ready = matches!(phase, LaunchAudit::Ready);
        let (body, timeout_message) = match phase {
            LaunchAudit::Ready => {
                let body = match self.locale {
                    Locale::En => {
                        let verb = if restoring { "restored" } else { "spawned" };
                        format!(
                            "{actor} {verb} {target} ({runtime}) in the committee `{committee}`."
                        )
                    }
                    Locale::Zh => {
                        let verb = if restoring { "恢复了" } else { "启动了" };
                        format!("{actor} 在委员会 `{committee}` {verb} {target}（{runtime}）。")
                    }
                };
                (body, "the spawn or restore audit was not acknowledged")
            }
            LaunchAudit::RolledBack { reason } => {
                let body = match self.locale {
                    Locale::En => {
                        let action = if restoring { "restore" } else { "spawn" };
                        format!(
                            "The {action} of {target} ({runtime}) by {actor} in committee `{committee}` was rolled back after publication failed. {reason}"
                        )
                    }
                    Locale::Zh => {
                        let action = if restoring { "恢复" } else { "启动" };
                        format!(
                            "{actor} 在委员会 `{committee}` 中{action} {target}（{runtime}）失败，操作已回滚。{reason}"
                        )
                    }
                };
                (body, "the launch rollback audit was not acknowledged")
            }
        };
        let mut event = auth.note(committee.clone(), body);
        if ready {
            event = event.with_meta(
                super::activity::LIFECYCLE,
                serde_json::json!(["started", reply.senator]),
            );
        }
        tokio::time::timeout(AUDIT_TIMEOUT, self.post(event))
            .await
            .map_err(|_| IpcError::new(codes::SERVER, timeout_message))??;
        Ok(())
    }

    /// Refuse, before anything starts, an alias another senator already
    /// goes by on `committee`, saying how to reach that one instead.
    async fn check_alias_free(
        &self,
        committee: &CommitteeId,
        alias: &SenatorName,
    ) -> Result<(), IpcError> {
        use senate_core::alias::AliasMatch;
        let aliases = tokio::time::timeout(ROLE_TIMEOUT, self.api.aliases())
            .await
            .map_err(|_| IpcError::new(codes::SERVER, "could not check available aliases in time"))?
            .map_err(server_error)?;
        let qualified = AliasName {
            committee: Some(committee.clone()),
            alias: alias.clone(),
        };
        let AliasMatch::One(holder) = aliases.resolve(&qualified, Some(committee)) else {
            return Ok(());
        };
        if aliases.alias_of(committee, &holder) != Some(alias) {
            // An older server's Senate-wide alias for a senator elsewhere.
            return Ok(());
        }
        let holder = senate_core::alias::short_name(&holder);
        Err(IpcError::bad_request(self.locale.pick(
            format!(
                "{alias} 已是委员会 {committee} 上 {holder} 的别名。用 `senate attach {alias}` 打开它，或换一个别名。"
            ),
            format!(
                "{alias} is already the alias of {holder} on {committee}. Open that one with \
                 `senate attach {alias}`, or choose another alias."
            ),
        )))
    }

    async fn next_spawn_alias(
        &self,
        runtime: Runtime,
        committee: &CommitteeId,
    ) -> Result<SenatorName, IpcError> {
        use senate_core::alias::AliasMatch;
        let (aliases, me) = tokio::time::timeout(ROLE_TIMEOUT, async {
            tokio::try_join!(self.api.aliases(), self.api.me())
        })
        .await
        .map_err(|_| IpcError::new(codes::SERVER, "could not check available aliases in time"))?
        .map_err(server_error)?;
        for ordinal in 1..=u32::MAX {
            let candidate = format!("{}{ordinal}", runtime.as_str())
                .parse::<SenatorName>()
                .map_err(IpcError::internal)?;
            let qualified = AliasName {
                committee: Some(committee.clone()),
                alias: candidate.clone(),
            };
            if matches!(aliases.resolve(&qualified, None), AliasMatch::NotAlias)
                && !me.people.is_person(&candidate)
                && !me.agents.contains(&candidate)
            {
                return Ok(candidate);
            }
        }
        Err(IpcError::bad_request("no default aliases available"))
    }

    async fn appoint_hosted_secretary(
        &self,
        auth: &HostAuthority,
        reply: &SpawnReply,
        desk: &SenatorName,
    ) -> Result<(), IpcError> {
        let appointment =
            self.api
                .add_secretary(desk, reply.senator.clone(), auth.acted_by.clone());
        match tokio::time::timeout(AUDIT_TIMEOUT, appointment).await {
            Ok(Ok(_)) => {
                self.refresh_me().await;
                Ok(())
            }
            outcome => {
                let error = match outcome {
                    Ok(Err(error)) => self.refusal(error),
                    _ => IpcError::new(
                        codes::SERVER,
                        "the secretary appointment was not acknowledged",
                    ),
                };
                Err(IpcError::new(
                    &error.code,
                    format!(
                        "{} was started and recorded in `{}`. Secretary appointment could not be confirmed. {}",
                        reply
                            .display_name
                            .as_deref()
                            .unwrap_or(reply.senator.as_str()),
                        reply.committee,
                        error.message,
                    ),
                ))
            }
        }
    }

    /// After the launch is audited, appoint the chair on the spawner's
    /// authority. A missing HTTP acknowledgement may hide a successful
    /// appointment, so the caller keeps this registered host running.
    pub(super) async fn appoint_hosted_chair(
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

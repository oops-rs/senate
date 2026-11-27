//! `senate cleanup` in the daemon: derive the plan, and apply the one the
//! person approved. The rules and the plan are in [`crate::cleanup`]; here
//! is where their facts come from and how each item is written.
//!
//! Every write is one the person can already make one at a time, made as
//! they make it: a seat through [`Shared::set_committee_seat`] (`senate
//! senator unseat`), an alias through the client's `remove_alias` (`senate
//! senator unalias`), a thread through an archived `status`. The server
//! decides whether the person may: what it refuses is reported, and the
//! run goes on with the rest.
//!
//! Applying derives the plan again and does what is in both
//! ([`Plan::agreed`]). It refuses outright, doing nothing, when the
//! approved plan was made for another Senate, person or log epoch, is older
//! than [`cleanup::PLAN_LIFETIME`], or names something no plan of this
//! Senate could: an unknown committee, a senator that is no coding
//! session's, a thread whose root is another event.
//!
//! It never stops a session, touches a running host or removes a committee.

use futures::{StreamExt, TryStreamExt};
use senate_core::alias::{CommitteeAliases, KnownAliases, display_name};
use senate_core::api::{Me, PresencePage, ThreadsQuery};
use senate_core::task::{ThreadSummary, WaitsOn};
use senate_core::{PresenceState, StatusPayload, StatusState};

use super::*;
use crate::cleanup::{
    self, Cause, CommitteePlan, Facts, META, Plan, Remaining, Report, Role, Scope, SeatPlan,
    SessionPlan, Standing, ThreadFacts, ThreadPlan,
};
use crate::host_approval::CloseCause;
use crate::ipc::SecretaryOp;

/// Threads asked for at a time.
const THREAD_PAGE: usize = 100;

/// One session on record here, and whether it is still there.
struct LocalSession {
    session: Session,
    host: Option<EventId>,
    alive: bool,
}

/// What this machine knows, read once for a plan.
struct Local {
    sessions: Vec<LocalSession>,
    paused: BTreeSet<SenatorName>,
    /// Messages waiting per seat; a seat with only a cursor waits for none.
    waiting: BTreeMap<(SenatorName, CommitteeId), usize>,
    /// Permission notices the daemon still follows, by their event id.
    notices: BTreeSet<EventId>,
}

/// One committee as the server shows it.
struct Seen {
    committee: CommitteeId,
    archived: bool,
    presence: PresencePage,
    chair: Option<SenatorName>,
}

impl Shared {
    pub(super) async fn on_cleanup_plan(&self, scope: Scope) -> Result<Plan, IpcError> {
        self.cleanup_plan(scope, chrono::Utc::now())
            .await
            .map(|(plan, _)| plan)
    }

    /// The plan for `scope` as things are at `now`, and who the Senate
    /// says the person is.
    async fn cleanup_plan(
        &self,
        scope: Scope,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(Plan, Me), IpcError> {
        let (me, aliases, secretaries) =
            tokio::try_join!(self.api.me(), self.api.aliases(), self.api.secretaries())
                .map_err(server_error)?;
        let KnownAliases::Scoped(aliases) = aliases else {
            return Err(IpcError::bad_request(self.locale.pick(
                "这个 Senate 的服务器版本太旧，不支持 senate cleanup；请先升级它。",
                "This Senate's server is too old for senate cleanup; upgrade it first.",
            )));
        };
        let every: Vec<(CommitteeId, bool)> = me
            .committees
            .iter()
            .map(|c| (c.clone(), false))
            .chain(me.archived.iter().map(|c| (c.clone(), true)))
            .collect();
        if let Some(unknown) = scope
            .committees
            .iter()
            .find(|c| !every.iter().any(|(known, _)| known == *c))
        {
            return Err(IpcError::bad_request(self.locale.pick(
                format!("没有委员会 {unknown}；`senate committees` 列出它们。"),
                format!("no committee {unknown}; `senate committees` lists them"),
            )));
        }
        let local = self.cleanup_local()?;
        // Every committee is read, whatever the scope: a senator connected
        // on one the run does not act on is alive all the same.
        let seen: Vec<Seen> = futures::stream::iter(every)
            .map(|(committee, archived)| self.cleanup_seen(committee, archived))
            .buffered(8)
            .try_collect()
            .await?;

        let mut facts: BTreeMap<SenatorName, Facts> = BTreeMap::new();
        for agent in &me.agents {
            facts.entry(agent.clone()).or_default().owned = true;
        }
        for local in &local.sessions {
            let entry = facts.entry(local.session.agent.clone()).or_default();
            entry.owned = true;
            entry.live |= local.alive;
            entry.last_active = entry.last_active.max(Some(local.session.last_seen));
        }
        for paused in &local.paused {
            facts.entry(paused.clone()).or_default().paused = true;
        }
        let mut chairs = BTreeSet::new();
        for committee in &seen {
            for presence in &committee.presence.agents {
                let entry = facts.entry(presence.agent.clone()).or_default();
                entry.online |= presence.state == PresenceState::Online;
                entry.paused |= presence.paused;
                entry.last_active = entry.last_active.max(presence.last_seen);
            }
            chairs.extend(committee.chair.clone());
        }
        for alias in aliases.iter() {
            facts.entry(alias.primary).or_default();
        }
        let desk: BTreeSet<SenatorName> = me
            .office
            .iter()
            .flat_map(|office| office.desk.iter().cloned())
            .chain(secretaries.values().flatten().cloned())
            .collect();
        let kept = self.cleanup_kept(&scope, &aliases, &facts)?;
        let people = me.people.clone();
        for (senator, entry) in &mut facts {
            entry.session_name = senator.is_session_name() && !people.is_person(senator);
            entry.office = me.office.as_ref().is_some_and(|office| {
                office.speaker_here() == Some(senator) || office.built_in.as_ref() == Some(senator)
            });
            entry.role = chairs.contains(senator) || desk.contains(senator);
            entry.kept = kept.contains(senator);
        }
        let standing: BTreeMap<&SenatorName, Standing> = facts
            .iter()
            .map(|(senator, facts)| (senator, cleanup::standing(facts, &scope, now)))
            .collect();
        let of = |senator: &SenatorName| standing.get(senator).copied().unwrap_or(Standing::Exempt);
        let roles = |senator: &SenatorName| {
            let mut roles = Vec::new();
            if chairs.contains(senator) {
                roles.push(Role::Chair);
            }
            if desk.contains(senator) {
                roles.push(Role::Secretary);
            }
            roles
        };

        let known = KnownAliases::Scoped(aliases.clone());
        let mut committees = Vec::new();
        for committee in seen.iter().filter(|c| scope.covers(&c.committee)) {
            let c = &committee.committee;
            let seated: BTreeSet<&SenatorName> = committee
                .presence
                .agents
                .iter()
                .map(|presence| &presence.agent)
                .collect();
            // Everything a senator has on this committee: a seat, aliases,
            // and what this machine still keeps for it there.
            let mut places: BTreeMap<&SenatorName, SeatPlan> = BTreeMap::new();
            let place = |senator: &SenatorName| SeatPlan {
                senator: senator.clone(),
                shown: display_name(None, Some(c), senator),
                seated: seated.contains(senator),
                aliases: Vec::new(),
                deliveries: 0,
                followed: false,
                last_active: facts.get(senator).and_then(|f| f.last_active),
                roles: roles(senator),
            };
            for senator in &seated {
                places.entry(senator).or_insert_with(|| place(senator));
            }
            let held: Vec<_> = aliases.iter().filter(|a| &a.committee == c).collect();
            for alias in &held {
                let Some((senator, _)) = facts.get_key_value(&alias.primary) else {
                    continue;
                };
                let entry = places.entry(senator).or_insert_with(|| place(senator));
                entry.shown = alias.alias.to_string();
                entry.aliases.push(alias.alias.clone());
            }
            for ((senator, on), waiting) in &local.waiting {
                if on == c {
                    let entry = places.entry(senator).or_insert_with(|| place(senator));
                    entry.deliveries += waiting;
                    entry.followed = true;
                }
            }
            let mut plan = CommitteePlan {
                committee: c.clone(),
                archived: committee.archived,
                seats: Vec::new(),
                roles: Vec::new(),
                recent: Vec::new(),
                threads: Vec::new(),
                remaining: Remaining::default(),
            };
            let mut unseated = 0;
            for (senator, place) in places {
                match of(senator) {
                    Standing::Dead if place.roles.is_empty() => {
                        unseated += usize::from(place.seated);
                        plan.seats.push(place);
                    }
                    // A role holder is dead only in a run that includes roles.
                    Standing::Dead => {
                        unseated += usize::from(place.seated);
                        plan.roles.push(place);
                    }
                    Standing::Role => plan.roles.push(place),
                    Standing::Recent => plan.recent.push(place),
                    Standing::Exempt | Standing::Alive | Standing::Kept => {}
                }
            }
            plan.remaining = Remaining {
                seats: seated.len() - unseated,
                live: seated
                    .iter()
                    .filter(|senator| of(senator) == Standing::Alive)
                    .count(),
                chair: committee
                    .chair
                    .as_ref()
                    .map(|chair| display_name(Some(&known), Some(c), chair)),
                chair_live: committee
                    .chair
                    .as_ref()
                    .is_some_and(|chair| of(chair) == Standing::Alive),
            };
            plan.threads = self
                .cleanup_threads(
                    c,
                    &scope,
                    &|senator| of(senator) == Standing::Dead,
                    &local.notices,
                    &people,
                    &known,
                    now,
                )
                .await?;
            committees.push(plan);
        }

        let mut sessions = Vec::new();
        for local in &local.sessions {
            let session = &local.session;
            let covered =
                scope.committees.is_empty() || session.committee().is_some_and(|c| scope.covers(c));
            if of(&session.agent) != Standing::Dead || !covered {
                continue;
            }
            let stale = session.state == SessionState::Stale;
            // What a forgotten record no longer has; one with none of it
            // was forgotten before.
            let left = stale
                || local.host.is_some()
                || session.socket.is_some()
                || session.token.is_some()
                || session.claude_pid.is_some()
                || self
                    .store
                    .stopped_on_purpose(&session.agent)
                    .map_err(internal)?;
            if left {
                sessions.push(SessionPlan {
                    session_id: session.session_id.clone(),
                    senator: session.agent.clone(),
                    committee: session.committee().cloned(),
                    hosted: local.host.is_some(),
                    stale,
                });
            }
        }
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        let plan = Plan {
            scope,
            senate: me.senate_path().to_string(),
            person: me.person.clone(),
            epoch: me.epoch.clone(),
            derived_at: now,
            committees,
            sessions,
        };
        Ok((plan, me))
    }

    /// The sessions on record, each checked against its process; who is
    /// paused; what waits for whom; and the notices still followed.
    fn cleanup_local(&self) -> Result<Local, IpcError> {
        let paths = Paths::new(&self.home);
        // Marks a live session whose process is gone as stale first.
        self.checked_live_sessions().map_err(internal)?;
        let mut sessions = Vec::new();
        for session in self.store.all_sessions().map_err(internal)? {
            let host = self
                .store
                .session_host(&session.session_id)
                .map_err(internal)?;
            let running = match &host {
                Some(host) => crate::host::running(&paths, host).map_err(IpcError::internal)?,
                None => false,
            };
            let alive = running
                || self.hosts.unpublished(&session.session_id)
                || match session.state {
                    SessionState::Live => true,
                    SessionState::Ended => false,
                    // Stale is a guess (a push failed): only a process that
                    // is gone makes it certain.
                    SessionState::Stale => {
                        session.claude_pid.is_some_and(session::pid_alive)
                            || (session.runtime == Runtime::Agy
                                && session::agy_conversation_open(&session.session_id)
                                    == Some(true))
                    }
                };
            sessions.push(LocalSession {
                session,
                host,
                alive,
            });
        }
        let paused = self
            .store
            .all_paused()
            .map_err(internal)?
            .into_iter()
            .map(|paused| paused.agent)
            .collect();
        let notices = self
            .store
            .hosted_approval_states()
            .map_err(internal)?
            .into_iter()
            .flat_map(|(_, state)| state.notices.into_iter().map(|notice| notice.id))
            .collect();
        Ok(Local {
            sessions,
            paused,
            waiting: self.store.waiting_by_seat().map_err(internal)?,
            notices,
        })
    }

    async fn cleanup_seen(&self, committee: CommitteeId, archived: bool) -> Result<Seen, IpcError> {
        let (presence, chair) =
            tokio::join!(self.api.presence(&committee), self.api.chair(&committee));
        let chair = match chair {
            Ok(info) => info.chair,
            // A committee without a chair was a 404 on older servers.
            Err(error) if error.status() == Some(404) => None,
            Err(error) => return Err(server_error(error)),
        };
        Ok(Seen {
            presence: presence.map_err(server_error)?,
            committee,
            archived,
            chair,
        })
    }

    /// The senators `--keep` names: by their own name, by an alias on the
    /// committee written with it, or by an alias wherever it is held. A
    /// name that finds nobody is refused: a typo must not unprotect.
    fn cleanup_kept(
        &self,
        scope: &Scope,
        aliases: &CommitteeAliases,
        known: &BTreeMap<SenatorName, Facts>,
    ) -> Result<BTreeSet<SenatorName>, IpcError> {
        let mut kept = BTreeSet::new();
        for name in &scope.keep {
            let mut found: Vec<SenatorName> = match &name.committee {
                Some(committee) => aliases
                    .get(committee, &name.alias)
                    .cloned()
                    .into_iter()
                    .collect(),
                None => aliases
                    .matches(&name.alias)
                    .into_iter()
                    .map(|alias| alias.primary)
                    .collect(),
            };
            if name.committee.is_none() && known.contains_key(&name.alias) {
                found.push(name.alias.clone());
            }
            if found.is_empty() {
                return Err(IpcError::bad_request(self.locale.pick(
                    format!("--keep {name}：没有这个名字的参议员或别名。"),
                    format!("--keep {name}: no senator or alias of that name"),
                )));
            }
            kept.extend(found);
        }
        Ok(kept)
    }

    /// The open threads of `committee` that [`cleanup::thread_cause`]
    /// archives, oldest first.
    #[allow(clippy::too_many_arguments)]
    async fn cleanup_threads(
        &self,
        committee: &CommitteeId,
        scope: &Scope,
        gone: &(dyn Fn(&SenatorName) -> bool + Sync),
        followed: &BTreeSet<EventId>,
        people: &People,
        aliases: &KnownAliases,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<ThreadPlan>, IpcError> {
        let older_than = scope.older_than_secs.map(Duration::from_secs);
        let mut open: Vec<ThreadSummary> = Vec::new();
        let mut before = None;
        loop {
            let page = self
                .api
                .thread_page(
                    committee,
                    ThreadsQuery {
                        include_discussions: true,
                        before,
                        limit: Some(THREAD_PAGE),
                        ..ThreadsQuery::default()
                    },
                )
                .await
                .map_err(server_error)?;
            let full = page.threads.len() >= THREAD_PAGE;
            before = page.threads.iter().map(|thread| thread.updated).min();
            open.extend(
                page.threads
                    .into_iter()
                    .filter(|thread| thread.state != StatusState::Archived),
            );
            if !full || before.is_none() {
                break;
            }
        }
        let mut plans = Vec::new();
        for thread in open {
            // Only a question from an agent to the chair, the Speaker or a
            // person can be a permission notice; its root says whether.
            let may_be_notice = thread.kind == Kind::Question
                && people.is_agent_address(&thread.from)
                && thread.on_behalf_of.is_none()
                && matches!(thread.to.as_slice(), [to] if matches!(to, Address::Chair | Address::Speaker) || people.is_person_address(to));
            let mut root = None;
            if may_be_notice {
                root = Some(self.fetch_event(committee, thread.root).await?);
            }
            let notice = root
                .as_ref()
                .is_some_and(|root| root.is_hosted_approval(people));
            let last_at = thread
                .activity
                .as_ref()
                .map_or(thread.since_ts, |activity| activity.last_at);
            let facts = ThreadFacts {
                notice,
                tracked: root
                    .as_ref()
                    .is_some_and(|root| followed.contains(&root.id)),
                from: &thread.from,
                on_behalf_of: thread.on_behalf_of.as_ref(),
                to: &thread.to,
                waits_on_agents: thread.waits() == Some(WaitsOn::Agents),
                waits_on_person: matches!(
                    thread.waits(),
                    Some(WaitsOn::Person | WaitsOn::Acceptance { .. })
                ),
                last_at,
            };
            let Some(cause) = cleanup::thread_cause(&facts, gone, older_than, now) else {
                continue;
            };
            let root = match root {
                Some(root) => root,
                None => self.fetch_event(committee, thread.root).await?,
            };
            let asker = thread.on_behalf_of.as_ref().unwrap_or(&thread.from);
            plans.push(ThreadPlan {
                root: thread.root,
                id: root.id,
                updated: thread.updated,
                cause,
                notice,
                // The server's own test of a task thread.
                task: matches!(thread.kind, Kind::Question | Kind::Request)
                    && thread.to.iter().any(|to| people.is_agent_address(to)),
                kind: thread.kind.to_string(),
                from: match asker.as_senator() {
                    Some(senator) => display_name(Some(aliases), Some(committee), senator),
                    None => asker.to_string(),
                },
                title: thread.title.chars().take(60).collect(),
                last_at,
            });
        }
        plans.sort_by_key(|plan| plan.root);
        Ok(plans)
    }

    /// Apply `approved`, the plan the person was shown (see the module's
    /// notes on what is done, left and refused).
    pub(super) async fn on_cleanup_apply(&self, approved: Plan) -> Result<Report, IpcError> {
        let now = chrono::Utc::now();
        let age = now.signed_duration_since(approved.derived_at);
        let lifetime =
            chrono::Duration::from_std(cleanup::PLAN_LIFETIME).unwrap_or(chrono::Duration::MAX);
        if age > lifetime || age < -chrono::Duration::minutes(1) {
            return Err(IpcError::bad_request(self.locale.pick(
                "这份清理计划已经过期；重新运行 `senate cleanup` 查看现在的计划。",
                "This cleanup plan is too old to apply; run `senate cleanup` again to see what it would do now.",
            )));
        }
        let (fresh, me) = self.cleanup_plan(approved.scope.clone(), now).await?;
        if (&fresh.senate, &fresh.person, &fresh.epoch)
            != (&approved.senate, &approved.person, &approved.epoch)
        {
            return Err(IpcError::bad_request(self.locale.pick(
                "这份清理计划是为另一个 Senate、另一个人或另一份日志做的；重新运行 `senate cleanup`。",
                "This cleanup plan was made for another Senate, person or log; run `senate cleanup` again.",
            )));
        }
        self.cleanup_check(&approved, &fresh).await?;
        let (plan, dropped, new) = approved.agreed(&fresh);
        let mut report = Report {
            dropped,
            new,
            ..Report::default()
        };
        self.cleanup_run(&plan, &me, &mut report).await;
        tracing::info!(
            failed = report.failed.len(),
            dropped = report.dropped.len(),
            sessions = report.sessions,
            "cleanup applied"
        );
        Ok(report)
    }

    /// Refuse a plan that names what no plan of this Senate could.
    async fn cleanup_check(&self, approved: &Plan, fresh: &Plan) -> Result<(), IpcError> {
        let not_a_plan = |what: String| {
            IpcError::bad_request(self.locale.pick(
                format!("这不是这个 Senate 的清理计划（{what}）；重新运行 `senate cleanup`。"),
                format!("This is not a cleanup plan of this Senate ({what}); run `senate cleanup` again."),
            ))
        };
        for committee in &approved.committees {
            let c = &committee.committee;
            let Some(now) = fresh.committees.iter().find(|f| &f.committee == c) else {
                return Err(not_a_plan(format!("committee {c}")));
            };
            if let Some(seat) = committee
                .seats
                .iter()
                .chain(&committee.roles)
                .find(|seat| !seat.senator.is_session_name())
            {
                return Err(not_a_plan(format!("{} on {c}", seat.senator)));
            }
            for thread in &committee.threads {
                let id = match now.threads.iter().find(|t| t.root == thread.root) {
                    Some(now) => now.id.clone(),
                    None => self.fetch_event(c, thread.root).await?.id,
                };
                if id != thread.id {
                    return Err(not_a_plan(format!("{c} #{}", thread.root)));
                }
            }
        }
        Ok(())
    }

    /// Do what `plan` holds. Each item stands alone: one that fails is
    /// recorded in `report` and the rest go on.
    async fn cleanup_run(&self, plan: &Plan, me: &Me, report: &mut Report) {
        let paths = Paths::new(&self.home);
        let stamp = u64::try_from(plan.derived_at.timestamp_millis()).unwrap_or_default();
        // History keeps the name each session went by: recorded for every
        // session of a senator this run acts on, before its alias goes.
        self.refresh_display_aliases().await;
        let acted: BTreeSet<&SenatorName> = plan
            .committees
            .iter()
            .flat_map(|committee| plan.acted(committee))
            .map(|seat| &seat.senator)
            .collect();
        for session in self.store.all_sessions().unwrap_or_default() {
            if acted.contains(&session.agent)
                && let Err(error) = self.remember_history_name(&session)
            {
                tracing::warn!(%error, "recording a session's display name");
            }
        }
        // A senator whose seat the server kept keeps its record too.
        let mut refused: BTreeSet<SenatorName> = BTreeSet::new();
        // Members of every committee first, then the role holders: the
        // server's successor rule then never hands a chair to a seat this
        // run removes.
        let mut places: Vec<(&CommitteeId, &SeatPlan)> = Vec::new();
        for committee in &plan.committees {
            places.extend(
                committee
                    .seats
                    .iter()
                    .map(|seat| (&committee.committee, seat)),
            );
        }
        if plan.scope.include_roles {
            for committee in &plan.committees {
                places.extend(
                    committee
                        .roles
                        .iter()
                        .map(|seat| (&committee.committee, seat)),
                );
            }
        }
        for (committee, seat) in places {
            if let Err(error) = self.cleanup_seat(committee, seat, stamp, report).await {
                report.fail(format!("{committee}: {}", seat.shown), error.message);
                refused.insert(seat.senator.clone());
            }
        }
        for committee in &plan.committees {
            for thread in &committee.threads {
                let event = match self.cleanup_archive(&committee.committee, thread, me, stamp) {
                    Ok(event) => event,
                    Err(error) => {
                        report.fail(
                            format!("{} #{}", committee.committee, thread.root),
                            error.message,
                        );
                        continue;
                    }
                };
                match self.post(event).await {
                    Ok(_) => {
                        *report
                            .committee(&committee.committee)
                            .threads
                            .entry(thread.cause)
                            .or_default() += 1;
                    }
                    Err(error) => report.fail(
                        format!("{} #{}", committee.committee, thread.root),
                        error.message,
                    ),
                }
            }
        }
        for session in &plan.sessions {
            if refused.contains(&session.senator) {
                continue;
            }
            match self.store.retire_session(&session.session_id, &paths) {
                Ok(Some(host)) => {
                    if let Some(host) = host
                        && let Err(error) = crate::host::remove_ended_host(&paths, &host)
                    {
                        tracing::warn!(%error, %host, "removing a forgotten session's host directory");
                    }
                    self.no_longer_working(&session.session_id);
                    report.sessions += 1;
                }
                Ok(None) => report
                    .dropped
                    .push(format!("session {}", session.session_id)),
                Err(error) => {
                    report.fail(format!("session {}", session.session_id), error.to_string())
                }
            }
        }
        *lock(&self.display_aliases) = None;
        self.refresh_me().await;
        self.committees_changed.notify_one();
    }

    /// One senator's place on one committee: its roles there, its seat, its
    /// aliases, and what this machine keeps for it. The seat comes first:
    /// when the server refuses it, the rest is left as it is.
    async fn cleanup_seat(
        &self,
        committee: &CommitteeId,
        seat: &SeatPlan,
        stamp: u64,
        report: &mut Report,
    ) -> Result<(), IpcError> {
        let senator = &seat.senator;
        if seat.roles.contains(&Role::Secretary) {
            let all = self.api.secretaries().await.map_err(server_error)?;
            for (principal, _) in all.iter().filter(|(_, list)| list.contains(senator)) {
                self.on_secretaries(
                    &SessionSelector::default(),
                    SecretaryOp::Remove,
                    Some(principal.clone()),
                    Some(senator.clone()),
                    None,
                )
                .await?;
            }
        }
        if seat.seated {
            let request = api::SetCommitteeSeat {
                senator: senator.clone(),
                action: senate_core::CommitteeSeatAction::Unseat,
                id: EventId::derived(stamp, &format!("cleanup:unseat:{committee}:{senator}")),
                acted_by: None,
            };
            self.set_committee_seat(committee, &request).await?;
            report.committee(committee).unseated += 1;
        }
        for alias in &seat.aliases {
            self.api
                .remove_alias(senator, alias, Some(committee))
                .await
                .map_err(server_error)?;
            report.committee(committee).aliases += 1;
        }
        if seat.followed {
            self.store
                .forget_agent(senator, std::slice::from_ref(committee))
                .map_err(internal)?;
            report.committee(committee).deliveries += seat.deliveries;
        }
        Ok(())
    }

    /// The archived `status` that closes `thread`, saying why beside it
    /// (`meta.cleanup`). A task thread is the person's own to archive. Any
    /// other root, a permission notice among them, the server lets only the
    /// Speaker's office close: the person writes it in person, as `senate
    /// desk close` does. The id is derived, so applying the same plan twice
    /// stores one event.
    fn cleanup_archive(
        &self,
        committee: &CommitteeId,
        thread: &ThreadPlan,
        me: &Me,
        stamp: u64,
    ) -> Result<NewEvent, IpcError> {
        let id = EventId::derived(stamp, &format!("cleanup:archive:{committee}:{}", thread.id));
        let words = thread.cause.words(self.locale);
        let why = serde_json::json!({"cause": thread.cause.as_str(), "by": me.person});
        let mut status = StatusPayload::new(StatusState::Archived);
        if thread.task && !thread.notice {
            status.reason = Some(words.to_owned());
            return Ok(Self::status_event(
                committee.clone(),
                self.person(),
                thread.root,
                &status,
                Some(id),
            )
            .with_meta(META, why));
        }
        let actor = desk_decision::actor(me).ok_or_else(|| {
            IpcError::new(
                codes::NOT_PRESIDING,
                self.locale.pick(
                    "只有 Speaker 本人能归档这个讨论串。",
                    "Only the Speaker in person can archive this thread.",
                ),
            )
        })?;
        let mut event = if thread.notice {
            // A notice takes a bare archive and nothing more: why it closed
            // goes beside the status, as the daemon's own close writes it.
            let closed = match thread.cause {
                Cause::SessionEnded => CloseCause::SessionEnded,
                Cause::AddresseeGone | Cause::AllGone | Cause::Stale => CloseCause::PromptGone,
            };
            let mut event = Self::status_event(
                committee.clone(),
                Address::Speaker,
                thread.root,
                &status,
                Some(id),
            )
            .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(thread.id))
            .with_meta(meta::APPROVAL_CLOSED, serde_json::json!(closed.as_str()));
            event.body = words.to_owned();
            event
        } else {
            status.reason = Some(words.to_owned());
            Self::status_event(
                committee.clone(),
                Address::Speaker,
                thread.root,
                &status,
                Some(id),
            )
        };
        event = event
            .with_meta(meta::ACTED_BY, serde_json::json!(actor))
            .with_meta(meta::DESK_PERSON, serde_json::json!(me.person))
            .with_meta(META, why);
        Ok(event)
    }
}

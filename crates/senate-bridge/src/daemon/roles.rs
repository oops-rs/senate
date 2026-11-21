//! The roles of one Senate as this machine sees them: the Speaker's office
//! (who works its desk, and which of this machine's agents does), each
//! committee's chair, the secretaries, and what an agent's message for
//! another committee becomes (the boundary rule).
//!
//! - **The desk.** A session presides when its agent works the Speaker's
//!   desk: the Speaker itself when it is an agent, else one of the
//!   Speaker's secretaries ([`senate_core::Office`], from `/v1/me`). What
//!   is put to the Speaker is pushed to the first desk worker this machine
//!   runs that has a live session (else the first one it runs), so it is
//!   pushed once. Everything a desk worker writes as `speaker` carries
//!   `meta.acted_by`. The office is in no committee's log, so the Senate
//!   says it on every stream, when the stream opens and whenever the desk
//!   changes ([`senate_core::api::SSE_OFFICE`]); this side then asks
//!   `/v1/me` again ([`Shared::on_office`]). A desk changed from anywhere
//!   is known here at once, with no request of this machine's.
//! - **The chair.** One per committee, a fact of its log
//!   ([`senate_core::ChairFold`]); this side asks `GET .../chair` when a
//!   stream opens and follows the log after. What is addressed to `chair`
//!   is pushed to the chair's session (an agent), or announced to the
//!   person (when the person chairs). When the chair moves, what is open on
//!   the chair's docket goes to the new chair and is withdrawn from the old
//!   one where it was not pushed yet ([`Shared::chair_pass`]).
//! - **Escalation** goes one step up: a member to its chair, the chair (or
//!   a committee with no chair) to the desk, a desk worker to the Speaker.
//! - **Secretaries** are registered with the Senate (`/v1/secretaries`);
//!   the companion ([`crate::companion`]) is registered as one for every
//!   agent it serves.

use senate_core::api::ChairInfo;
use senate_core::{
    Awaiting, ChairFold, ChairPayload, ChairSource, CommitteeSeatAction, CommitteeSeatChange,
    DocketItem, Office, Origin, PresencePayload, SenatorId, SenatorKind,
};

use super::*;
use crate::ipc::{
    ChairDocketReply, DeskRole, EscalateReply, EscalatedTo, Forwarded, RolesReply,
    SecretariesReply, SecretaryOp, SendReply,
};
use crate::paths::HOSTED_KEY;

/// Bound on asking the server for a chair or for `/v1/me` while a request
/// waits.
const ROLE_TIMEOUT: Duration = Duration::from_secs(2);

/// What this side knows of one committee's chair.
#[derive(Debug, Default)]
pub(crate) struct ChairCache {
    /// As the server said (`GET .../chair`), kept current from the log.
    known: Option<(SenatorName, ChairSource)>,
    /// The log as this side saw it.
    fold: ChairFold,
    /// Whether the fold saw the committee from its first event.
    whole: Option<bool>,
    /// The last senator this side knew to chair it.
    held: Option<SenatorName>,
}

impl ChairCache {
    fn observe(&mut self, event: &Event) -> bool {
        if event.seq <= self.fold.head() {
            return false;
        }
        if self.whole.is_none() {
            self.whole = Some(event.seq == Seq(1));
        }
        self.fold.observe(event);
        if let Some(change) = CommitteeSeatChange::of(event) {
            if change.action == CommitteeSeatAction::Unseat
                && self
                    .known
                    .as_ref()
                    .is_some_and(|(chair, _)| chair == &change.senator)
            {
                self.known = None;
            }
            return true;
        }
        if let Some(chair) = ChairPayload::of(event) {
            self.known = Some((chair.senator, ChairSource::Set));
        } else if event.kind == Kind::Presence
            && let Some(p) = PresencePayload::of(event)
            && matches!(&self.known, Some((_, ChairSource::Creator)))
        {
            self.known = Some((p.agent, ChairSource::FirstAgent));
        }
        false
    }

    /// Whether the chair passed from one senator to another since this was
    /// last asked: the first chair known is no move, and neither is a
    /// committee left with none for a while.
    fn handed_over(&mut self) -> bool {
        let Some(now) = self.chair() else {
            return false;
        };
        if self.held.as_ref() == Some(&now) {
            return false;
        }
        self.held.replace(now).is_some()
    }

    fn chair(&self) -> Option<SenatorName> {
        if let Some((chair, _)) = &self.known {
            return Some(chair.clone());
        }
        if self.whole == Some(true) {
            return self.fold.chair().map(|(c, _)| c.clone());
        }
        None
    }
}

impl Shared {
    // ---- the office --------------------------------------------------------

    /// Record the Speaker's office and the child Senates `/v1/me` named.
    pub(crate) fn note_office(&self, me: &senate_core::api::Me) {
        self.set_office(me.office.clone());
        *lock(&self.children) = me.children.clone();
    }

    /// Record the Speaker's office. Another one than before is told to
    /// whoever watches ([`WatchEvent::Roles`]): the sessions of the agents
    /// on the desk, and of those off it now, have other tools.
    fn set_office(&self, now: Option<Office>) {
        {
            let mut office = lock(&self.office);
            if *office == now {
                return;
            }
            tracing::info!(office = ?now, "the Speaker's office");
            *office = now;
        }
        self.publish(WatchEvent::Roles);
    }

    /// The Speaker's office, as `/v1/me` last said.
    pub(crate) fn office(&self) -> Option<Office> {
        lock(&self.office).clone()
    }

    /// Ask `/v1/me` again (bounded), and record what it says. False when
    /// it did not answer: what was known stays.
    pub(crate) async fn refresh_me(&self) -> bool {
        let Ok(Ok(me)) = tokio::time::timeout(ROLE_TIMEOUT, self.api.me()).await else {
            return false;
        };
        self.note_triaged(me.triaged);
        self.note_senate(&me);
        self.note_office(&me);
        self.note_speaker(me.speaker);
        true
    }

    /// The Senate said its Speaker's office on a stream (`data`: the
    /// office as JSON, or `null`), as it does when a stream opens and when
    /// the desk changes. When that is not the office known here, the rest
    /// of what `/v1/me` says may have moved with it (who works the desk
    /// first, whether roots are triaged), so `/v1/me` is asked again; when
    /// it does not answer, the office told is kept as it was told.
    pub(crate) async fn on_office(&self, data: &str) {
        let told: Option<Office> = match serde_json::from_str(data) {
            Ok(office) => office,
            Err(e) => {
                tracing::debug!("an office this build cannot read: {e}");
                return;
            }
        };
        if self.office() == told || self.refresh_me().await {
            return;
        }
        self.set_office(told);
    }

    /// How `agent`, one this machine runs, works the Speaker's desk, if it
    /// does. With a server that names no office, the older seat says.
    pub(crate) fn desk_role(&self, agent: &SenatorName) -> Option<DeskRole> {
        if !self.seating().owns(agent) {
            return None;
        }
        match self.office() {
            Some(office) => {
                if office.speaker_here() == Some(agent) && office.works_desk(agent) {
                    Some(DeskRole::Speaker)
                } else if office.agent_workers().any(|w| w == agent) {
                    Some(DeskRole::Secretary)
                } else {
                    None
                }
            }
            None => self
                .speaker_seat()
                .filter(|seat| seat.senator() == Some(agent))
                .map(|_| DeskRole::Speaker),
        }
    }

    /// The desk's agent workers, in order.
    fn desk_workers(&self) -> Vec<SenatorName> {
        match self.office() {
            Some(office) => office.agent_workers().cloned().collect(),
            None => self
                .speaker_seat()
                .and_then(|s| s.senator().cloned())
                .into_iter()
                .collect(),
        }
    }

    /// The desk workers with a live session on this machine.
    fn present_workers(&self) -> Vec<SenatorName> {
        let live: BTreeSet<SenatorName> = self
            .store
            .live_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.agent)
            .collect();
        self.desk_workers()
            .into_iter()
            .filter(|w| live.contains(w))
            .collect()
    }

    /// The one of this machine's agents what is put to the Speaker is
    /// pushed to: the first desk worker it runs with a live session, else
    /// the first it runs at all. The first present acts; pushing to one
    /// keeps it from being pushed twice.
    pub(crate) fn desk_pusher(&self) -> Option<SenatorName> {
        let mine = self.served_agents().unwrap_or_default();
        let workers: Vec<SenatorName> = self
            .desk_workers()
            .into_iter()
            .filter(|w| mine.contains(w) && self.seating().owns(w))
            .collect();
        let present = self.present_workers();
        workers
            .iter()
            .find(|w| present.contains(w))
            .or(workers.first())
            .cloned()
    }

    /// Whether the person this machine runs for is the Speaker.
    /// The person is the Speaker of this Senate: as its own senator (a
    /// root), or as a senator of the Senate above (a child on their
    /// machine).
    pub(crate) fn person_is_speaker(&self) -> bool {
        self.office().is_some_and(|o| {
            o.speaker.name().name() == &self.config.person
                && !matches!(o.kind, SenatorKind::Agent | SenatorKind::BuiltIn)
        })
    }

    /// Something addressed to the person by a role they hold: the chair of
    /// its committee, or the Speaker's desk when they are the Speaker.
    /// Roots only; not their own words.
    pub(crate) fn role_attention(&self, event: &Event) -> Option<senate_core::Attention> {
        let person = Address::senior(&self.config.person);
        if event.thread.is_some()
            || event.from == person
            || !matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        {
            return None;
        }
        let chair = event.to.contains(&Address::Chair)
            && self
                .chair_of(&event.committee)
                .is_some_and(|c| c.name() == &self.config.person);
        let desk = event.to.contains(&Address::Speaker)
            && event.from != Address::Speaker
            && self.person_is_speaker();
        (chair || desk).then(|| senate_core::Attention {
            escalated: false,
            text: event.body.clone(),
        })
    }

    // ---- chairs --------------------------------------------------------------

    /// Follow the log for `event`'s committee's chair.
    pub(crate) fn observe_chair(&self, event: &Event) -> bool {
        let (seats_changed, moved) = {
            let mut chairs = lock(&self.chairs);
            let cache = chairs.entry(event.committee.clone()).or_default();
            let before = cache.chair();
            let seats_changed = cache.observe(event);
            if cache.handed_over() {
                lock(&self.chair_moved).insert(event.committee.clone());
                self.work.notify_one();
            }
            (seats_changed, cache.chair() != before)
        };
        if moved {
            // The old chair's session and the new one's have other tools.
            self.publish(WatchEvent::Roles);
        }
        seats_changed
    }

    /// Ask the server who chairs `committee` (a stream opened).
    pub(crate) async fn seed_chair(&self, committee: &CommitteeId) {
        match tokio::time::timeout(ROLE_TIMEOUT, self.api.chair(committee)).await {
            Ok(Ok(info)) => self.note_chair(&info),
            Ok(Err(e)) => tracing::debug!(%committee, "the chair is not known yet: {e}"),
            Err(_) => {
                tracing::debug!(%committee, "the chair is not known yet: the server was slow")
            }
        }
    }

    fn note_chair(&self, info: &ChairInfo) {
        let moved = {
            let mut chairs = lock(&self.chairs);
            let cache = chairs.entry(info.committee.clone()).or_default();
            let before = cache.chair();
            cache.known = info
                .chair
                .clone()
                .map(|c| (c, info.source.unwrap_or(ChairSource::Set)));
            if cache.handed_over() {
                lock(&self.chair_moved).insert(info.committee.clone());
                self.work.notify_one();
            }
            cache.chair() != before
        };
        if moved {
            self.publish(WatchEvent::Roles);
        }
    }

    /// The chair of `committee`, as far as this side knows.
    pub(crate) fn chair_of(&self, committee: &CommitteeId) -> Option<SenatorName> {
        lock(&self.chairs)
            .get(committee)
            .and_then(ChairCache::chair)
    }

    /// Every chair this side knows.
    pub(crate) fn known_chairs(&self) -> Vec<(CommitteeId, SenatorName)> {
        lock(&self.chairs)
            .iter()
            .filter_map(|(c, cache)| cache.chair().map(|s| (c.clone(), s)))
            .collect()
    }

    /// The chair of `committee` now: asked of the server, else as known.
    pub(crate) async fn current_chair(&self, committee: &CommitteeId) -> Option<SenatorName> {
        self.seed_chair(committee).await;
        self.chair_of(committee)
    }

    /// Whether `event` is addressed to `chair` on its committee and `agent`
    /// chairs it: pushed to that agent's session. Not its own words.
    pub(crate) fn for_chair(&self, agent: &SenatorName, event: &Event) -> bool {
        event.to.contains(&Address::Chair)
            && event.from != Address::senator(agent)
            && event.from != Address::Chair
            && !self.people().is_person(agent)
            && self.chair_of(&event.committee).as_ref() == Some(agent)
    }

    /// The session's committee, refused unless its agent chairs it.
    async fn chairing(
        &self,
        selector: &SessionSelector,
    ) -> Result<(Session, CommitteeId), IpcError> {
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        let chair = self.current_chair(&committee).await;
        if chair.as_ref() != Some(&session.agent) {
            return Err(IpcError::new(
                codes::NOT_CHAIRING,
                roles_text::not_chairing(&session.agent, &committee, chair.as_ref(), self.locale),
            ));
        }
        Ok((session, committee))
    }

    /// `addr` on `committee` of this Senate, as a complete qualified name
    /// when the Senate's path is known.
    pub(crate) fn locator_of(&self, committee: &CommitteeId, addr: Address) -> Locator {
        match self.senate_path() {
            Some(path) => Locator::at(path, committee.clone(), addr),
            None => Locator::on(committee.clone(), addr),
        }
    }

    pub(crate) async fn on_chair_docket(
        &self,
        selector: &SessionSelector,
    ) -> Result<ChairDocketReply, IpcError> {
        let (session, committee) = self.chairing(selector).await?;
        let page = self
            .api
            .chair_docket(&committee)
            .await
            .map_err(server_error)?;
        let mut checkpoints = Vec::new();
        let mut away: BTreeSet<SenatorName> = BTreeSet::new();
        for item in &page.items {
            if let Awaiting::Away { agents } = &item.awaiting {
                away.extend(agents.iter().cloned());
            }
        }
        for agent in away {
            if let Some(record) = self
                .api
                .latest_checkpoint(&committee, &agent)
                .await
                .map_err(server_error)?
            {
                checkpoints.push(record);
            }
        }
        Ok(ChairDocketReply {
            chair: session.agent,
            page,
            checkpoints,
        })
    }

    async fn chair_item(&self, committee: &CommitteeId, seq: Seq) -> Result<DocketItem, IpcError> {
        let page = self
            .api
            .chair_docket(committee)
            .await
            .map_err(server_error)?;
        page.items
            .into_iter()
            .find(|i| i.event.seq == seq)
            .ok_or_else(|| {
                IpcError::bad_request(roles_text::not_on_chair_docket(committee, seq, self.locale))
            })
    }

    /// The chair routes an item to one member of its committee: relayed in
    /// its own name, with the asker as the origin, so the answer goes back
    /// to the asker's thread.
    pub(crate) async fn on_chair_route(
        &self,
        selector: &SessionSelector,
        seq: Seq,
        to: SenatorName,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let (session, committee) = self.chairing(selector).await?;
        let item = self.chair_item(&committee, seq).await?;
        match &item.awaiting {
            Awaiting::Pick { candidates, .. } if !candidates.contains(&to) => {
                return Err(IpcError::bad_request(roles_text::not_a_candidate(
                    &to,
                    candidates,
                    self.locale,
                )));
            }
            Awaiting::Away { .. } => {
                return Err(IpcError::bad_request(roles_text::never_reroute(
                    self.locale,
                )));
            }
            _ if self.people().is_person(&to) || to == session.agent => {
                return Err(IpcError::bad_request(roles_text::route_to_a_member(
                    &to,
                    self.locale,
                )));
            }
            _ => {}
        }
        let root = item.event;
        // Inside one committee: the relay names the item it routes; no
        // origin, which is for a message from elsewhere.
        let mut event = NewEvent::new(
            committee,
            root.kind.clone(),
            Address::senator(&session.agent),
            root.body,
        )
        .to(Address::senator(&to))
        .with_meta(meta::RELAYED_FROM, serde_json::json!(seq.0));
        if let Some(refs) = root.refs {
            event = event.with_refs(refs);
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// The chair answers a question to an absent member from that member's
    /// own latest checkpoint, in its own name, saying for whom.
    pub(crate) async fn on_chair_cover(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        body: String,
        agent: Option<SenatorName>,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let (session, committee) = self.chairing(selector).await?;
        let item = self.chair_item(&committee, thread).await?;
        let Awaiting::Away { agents } = &item.awaiting else {
            return Err(IpcError::bad_request(roles_text::not_away(self.locale)));
        };
        let member = match (agent, agents.as_slice()) {
            (Some(a), _) if agents.contains(&a) => a,
            (Some(a), _) => {
                return Err(IpcError::bad_request(roles_text::not_its_question(
                    &a,
                    agents,
                    self.locale,
                )));
            }
            (None, [one]) => one.clone(),
            (None, several) => {
                return Err(IpcError::bad_request(roles_text::name_which(
                    several,
                    self.locale,
                )));
            }
        };
        let Some(checkpoint) = self
            .api
            .latest_checkpoint(&committee, &member)
            .await
            .map_err(server_error)?
        else {
            return Err(IpcError::bad_request(roles_text::no_checkpoint(
                &member,
                self.locale,
            )));
        };
        let root = item.event;
        let asker = root.on_behalf_of.clone().unwrap_or(root.from.clone());
        let text = format!(
            "{}\n{body}",
            roles_text::covering_line(&member, checkpoint.seq, self.locale)
        );
        let mut event = NewEvent::new(
            committee,
            Kind::Answer,
            Address::senator(&session.agent),
            text,
        )
        .to(asker)
        .in_thread(root.seq)
        .with_refs(Refs {
            commit: Some(checkpoint.checkpoint.commit.clone()),
            path: None,
            branch: Some(checkpoint.checkpoint.branch.clone()),
        });
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    // ---- covering for a principal -----------------------------------------

    /// Whether `event` is an answer one of `me`'s secretaries gave in its
    /// name while it was away, to be pushed to `me` when it is back (not
    /// the built-in session's, whose engine writes its own hand-back note,
    /// nor this machine's companion's, whose question is settled where it
    /// runs).
    pub(crate) fn answered_in_my_name(&self, me: &SenatorName, event: &Event) -> bool {
        let built_in = self.office().and_then(|o| o.built_in);
        answered_for_me_by_a_secretary(event, me, &self.companion_config.name)
            && format::secretary_answer(event)
                .is_some_and(|(_, secretary)| Some(secretary) != built_in.as_ref())
    }

    /// Something happened that may change what a local agent covers as a
    /// secretary on `event`'s committee: a question to another senator
    /// (it may be away, and `agent` its secretary), or, while a covered
    /// item is waiting there, an answer, a presence change or a status.
    pub(crate) fn note_cover_change(&self, agent: &SenatorName, event: &Event) {
        let people = self.people();
        let asked = event.thread.is_none()
            && event.kind == Kind::Question
            && event.from != Address::senator(agent)
            && event.to.iter().any(|a| {
                a.as_senator()
                    .is_some_and(|s| s != agent && !people.is_person(s))
            });
        let may_withdraw = matches!(event.kind, Kind::Answer | Kind::Presence | Kind::Status)
            && lock(&self.cover_open)
                .iter()
                .any(|(_, committee, _)| committee == &event.committee);
        if asked || may_withdraw {
            lock(&self.cover_dirty).insert(event.committee.clone());
            self.work.notify_one();
        }
    }

    /// Bring to the session of each local agent the questions it covers as
    /// its principal's secretary, and withdraw what no longer needs it.
    /// The server says which: its docket lists, for a committee, the
    /// questions to an away agent that an agent secretary with a live
    /// connection covers ([`senate_core::DocketPage::covered`], the first
    /// present secretary in the principal's order). The question is
    /// queued for that secretary only when its session is here; it is
    /// withdrawn when it is no longer covered (the principal is back, or
    /// someone answered), unless already pushed.
    ///
    /// Returns when to look again: while an item queued for a secretary
    /// is not yet pushed, the principal may come back without a word in
    /// the log, so it is looked at again every heartbeat.
    pub(crate) async fn cover_pass(&self) -> Option<Instant> {
        let committees: Vec<CommitteeId> = std::mem::take(&mut *lock(&self.cover_dirty))
            .into_iter()
            .collect();
        for committee in committees {
            let page = match tokio::time::timeout(ROLE_TIMEOUT, self.api.docket(&committee)).await {
                Ok(Ok(page)) => page,
                Ok(Err(e)) => {
                    tracing::debug!(%committee, "no docket to cover from: {e}");
                    continue;
                }
                Err(_) => {
                    tracing::debug!(%committee, "no docket to cover from: the server was slow");
                    continue;
                }
            };
            if let Err(e) = self.cover_committee(&committee, &page).await {
                tracing::warn!(%committee, "covering for a principal: {e}");
            }
        }
        let open: BTreeSet<CommitteeId> = lock(&self.cover_open)
            .iter()
            .map(|(_, committee, _)| committee.clone())
            .collect();
        if open.is_empty() {
            return None;
        }
        lock(&self.cover_dirty).extend(open);
        Some(Instant::now() + self.timing.heartbeat)
    }

    async fn cover_committee(
        &self,
        committee: &CommitteeId,
        page: &senate_core::DocketPage,
    ) -> anyhow::Result<()> {
        let live: BTreeSet<SenatorName> = self
            .store
            .live_sessions()?
            .into_iter()
            .filter(|s| s.committee() == Some(committee))
            .map(|s| s.agent)
            .collect();
        let mine = self.served_agents()?;
        // What is covered and has not been queued: bring it in.
        for (root, secretary) in &page.covered {
            if !(mine.contains(secretary) && self.seating().owns(secretary))
                || !live.contains(secretary)
            {
                continue;
            }
            let key = (secretary.clone(), committee.clone(), *root);
            if self.store.item(secretary, committee, *root)?.is_some() {
                continue;
            }
            let mut event = match self.fetch_event(committee, *root).await {
                Ok(event) => event,
                Err(e) => {
                    tracing::debug!(%committee, %root, "cannot read the question to cover: {}", e.message);
                    continue;
                }
            };
            let Some(principal) = event
                .to
                .iter()
                .filter_map(Address::as_senator)
                .find(|s| *s != secretary)
                .cloned()
            else {
                continue;
            };
            event
                .meta
                .insert(format::COVER_META.into(), principal.to_string().into());
            if self.store.enqueue(secretary, &event)? {
                tracing::info!(
                    %committee,
                    seq = %root,
                    secretary = %secretary,
                    %principal,
                    "a question to an away agent is queued for its secretary"
                );
                lock(&self.cover_open).insert(key);
                self.work.notify_one();
            }
        }
        // What is queued for a secretary here but no longer covered, and
        // not yet pushed: withdraw it.
        let open: Vec<ItemKey> = lock(&self.cover_open)
            .iter()
            .filter(|(_, c, _)| c == committee)
            .cloned()
            .collect();
        for key in open {
            let (secretary, _, root) = &key;
            let waiting = self
                .store
                .item(secretary, committee, *root)?
                .is_some_and(|i| matches!(i.status, ItemStatus::Pending | ItemStatus::Queued));
            if !waiting {
                lock(&self.cover_open).remove(&key);
            } else if page.covered.get(root) != Some(secretary) {
                self.store
                    .cancel(Some(secretary), committee, *root, false, "withdrawn")?;
                lock(&self.cover_open).remove(&key);
                tracing::info!(
                    %committee,
                    seq = %root,
                    %secretary,
                    "the question is no longer the secretary's to cover; withdrawn"
                );
            }
        }
        Ok(())
    }

    /// Hand what waits on `chair` to the new chair of each committee whose
    /// chair moved. Who a message to `chair` is for is otherwise decided
    /// once, as each agent's stream passes it ([`Shared::for_chair`]): a
    /// stream that passed it while another chaired never comes back to it.
    /// So on a move, every item on the chair's docket (the roots addressed
    /// to `chair` that the chair has not answered and that are not
    /// archived) is queued for the new chair when it is an agent served
    /// here, unless it wrote it; and what is still queued, not pushed, for any other agent
    /// only as `chair` is withdrawn ([`crate::state::CHAIR_MOVED`]). An item the
    /// new chair lost on an earlier move is queued again; one it was pushed
    /// already is not.
    pub(crate) async fn chair_pass(&self) {
        let committees = std::mem::take(&mut *lock(&self.chair_moved));
        for committee in committees {
            if let Err(e) = self.hand_over(&committee).await {
                // Looked at again on the next pass.
                tracing::warn!(%committee, "handing the chair's items over: {e}");
                lock(&self.chair_moved).insert(committee);
            }
        }
    }

    async fn hand_over(&self, committee: &CommitteeId) -> anyhow::Result<()> {
        let Some(chair) = self.chair_of(committee) else {
            return Ok(());
        };
        let addressed = |event: &Event, agent: &SenatorName| {
            event.to.contains(&Address::Chair) && !event.is_directed_to(&Address::senator(agent))
        };
        for item in self.store.waiting_on(committee)? {
            if item.agent != chair && addressed(&item.event, &item.agent) {
                let gone = self.store.cancel(
                    Some(&item.agent),
                    committee,
                    item.event.seq,
                    false,
                    crate::state::CHAIR_MOVED,
                )?;
                if !gone.is_empty() {
                    tracing::info!(%committee, seq = %item.event.seq, agent = %item.agent, %chair, "the chair moved; withdrawn");
                }
            }
        }
        if self.people().is_person(&chair)
            || !(self.served_agents()?.contains(&chair) && self.seating().owns(&chair))
        {
            return Ok(());
        }
        let page = tokio::time::timeout(ROLE_TIMEOUT, self.api.chair_docket(committee))
            .await
            .map_err(|_| anyhow::anyhow!("the server was slow"))??;
        let mut queued = false;
        for item in page.items {
            let event = item.event;
            if self.for_chair(&chair, &event) && self.store.enqueue_for_chair(&chair, &event)? {
                tracing::info!(%committee, seq = %event.seq, %chair, "the chair moved; handed to the new chair");
                queued = true;
            }
        }
        if queued {
            self.cap_unbound(&chair, committee)?;
            self.work.notify_one();
        }
        Ok(())
    }

    /// Put the absent principal's own latest checkpoint and notes on each
    /// event queued for `agent` to cover, so the push carries what to
    /// answer from ([`format::COVER_NOTES_META`]). None found: the push
    /// says so.
    pub(crate) async fn with_cover_notes(&self, events: &mut [Event]) {
        for event in events {
            let Some(principal) = format::cover_principal(event) else {
                continue;
            };
            match tokio::time::timeout(
                ROLE_TIMEOUT,
                self.api.latest_checkpoint(&event.committee, &principal),
            )
            .await
            {
                Ok(Ok(Some(record))) => {
                    if let Ok(value) = serde_json::to_value(&record) {
                        event.meta.insert(format::COVER_NOTES_META.into(), value);
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    tracing::debug!(committee = %event.committee, %principal, "no checkpoint to cover from: {e}")
                }
                Err(_) => {
                    tracing::debug!(committee = %event.committee, %principal, "no checkpoint to cover from: the server was slow")
                }
            }
        }
    }

    /// A secretary answers a question to the agent it is the secretary of,
    /// while that agent is away: in the agent's name (`on_behalf_of`,
    /// marked as a takeover), signed by the secretary, from the agent's
    /// own latest checkpoint. A question only, and only while the server
    /// still says this session's agent is the one covering it (the agent
    /// away, the question unanswered, no secretary before it present).
    pub(crate) async fn on_secretary_cover(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        body: String,
        agent: Option<SenatorName>,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        let root = self.task_root(&committee, thread).await?;
        if root.kind != Kind::Question {
            return Err(IpcError::bad_request(roles_text::never_a_request(
                self.locale,
            )));
        }
        let people = self.people();
        let targets: Vec<SenatorName> = root
            .to
            .iter()
            .filter_map(Address::as_senator)
            .filter(|s| **s != session.agent && !people.is_person(s))
            .cloned()
            .collect();
        let principal = match (agent, targets.as_slice()) {
            (Some(a), _) if targets.contains(&a) => a,
            (Some(a), _) => {
                return Err(IpcError::bad_request(roles_text::not_its_question(
                    &a,
                    &targets,
                    self.locale,
                )));
            }
            (None, [one]) => one.clone(),
            (None, several) => {
                return Err(IpcError::bad_request(roles_text::name_which(
                    several,
                    self.locale,
                )));
            }
        };
        let page = self.api.docket(&committee).await.map_err(server_error)?;
        if page.covered.get(&root.seq) != Some(&session.agent) {
            return Err(IpcError::bad_request(roles_text::not_yours_to_cover(
                &principal,
                self.locale,
            )));
        }
        let Some(checkpoint) = self
            .api
            .latest_checkpoint(&committee, &principal)
            .await
            .map_err(server_error)?
        else {
            return Err(IpcError::bad_request(roles_text::no_checkpoint_to_cover(
                &principal,
                self.locale,
            )));
        };
        let asker = root.on_behalf_of.clone().unwrap_or(root.from.clone());
        let mut event = NewEvent::new(
            committee,
            Kind::Answer,
            Address::senator(&session.agent),
            body,
        )
        .to(asker)
        .in_thread(root.seq)
        .on_behalf_of(Address::senator(&principal))
        .with_meta(meta::TAKEOVER, serde_json::json!(true))
        .with_refs(Refs {
            commit: Some(checkpoint.checkpoint.commit.clone()),
            path: None,
            branch: Some(checkpoint.checkpoint.branch.clone()),
        });
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// The chair puts an item one step up: to the Speaker's desk, with the
    /// asker as the origin.
    pub(crate) async fn on_chair_escalate(
        &self,
        selector: &SessionSelector,
        seq: Seq,
        reason: String,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if reason.trim().is_empty() {
            return Err(IpcError::bad_request("`reason` is empty"));
        }
        let (session, committee) = self.chairing(selector).await?;
        let item = self.chair_item(&committee, seq).await?;
        let root = item.event;
        let asker = root.on_behalf_of.clone().unwrap_or(root.from.clone());
        let origin = Origin {
            locator: self.locator_of(&committee, asker),
            thread: root.seq,
        };
        let mut event = NewEvent::new(
            committee,
            Kind::Question,
            Address::senator(&session.agent),
            reason,
        )
        .to(Address::Speaker)
        .with_meta(meta::ORIGIN, origin.value());
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    // ---- the desk's own requests ---------------------------------------------

    /// Pass a desk item meant for another committee on: a new root at the
    /// intended seat from `speaker` with the asker as its origin, and a note
    /// in the asker's thread saying where it went.
    pub(crate) async fn on_forward(
        &self,
        acted_by: &SenatorName,
        committee: CommitteeId,
        seq: Seq,
        id: Option<EventId>,
    ) -> Result<Forwarded, IpcError> {
        let (item, _) = self.docket_item(&committee, seq).await?;
        let Some(intended) = item.intended.clone() else {
            return Err(IpcError::bad_request(roles_text::nothing_intended(
                self.locale,
            )));
        };
        let here = self.senate_path();
        let elsewhere = match (intended.senate(), &here) {
            (None, _) => false,
            (Some(theirs), Some(ours)) => {
                theirs.clone().absolute() != ours.clone().absolute() && !ours.ends_with(theirs)
            }
            (Some(_), None) => true,
        };
        let above = elsewhere.then(|| self.above_for(&intended)).flatten();
        if elsewhere && above.is_none() {
            return Err(IpcError::bad_request(roles_text::not_in_this_senate(
                &intended,
                self.locale,
            )));
        }
        let target = intended.committee().cloned().unwrap_or(committee.clone());
        let to = intended.address().clone();
        let root = item.event;
        let asker = root.on_behalf_of.clone().unwrap_or(root.from.clone());
        let origin = Origin {
            locator: self.locator_of(&committee, asker.clone()),
            thread: root.seq,
        };
        let acted = serde_json::json!(acted_by.to_string());
        // Above, the office speaks as this child's Speaker for the
        // committee it was asked on; the answer there is relayed down by
        // the machine's uplink.
        let from = match &above {
            Some((name, _)) => Address::child(name.clone(), committee.clone()),
            None => Address::Speaker,
        };
        let mut forwarded = NewEvent::new(target, root.kind.clone(), from, root.body)
            .to(to)
            .with_meta(meta::ORIGIN, origin.value())
            .with_meta(meta::ACTED_BY, acted.clone());
        if let Some(refs) = root.refs {
            forwarded = forwarded.with_refs(refs);
        }
        if let Some(id) = id {
            forwarded = forwarded.with_id(id);
        }
        let posted = match &above {
            Some((_, client)) => {
                let r = client
                    .append(&forwarded)
                    .await
                    .map_err(|e| self.refusal(e))?;
                Posted {
                    committee: r.event.committee,
                    seq: r.event.seq,
                    to: r.event.to,
                    thread: r.event.thread,
                    duplicate: r.duplicate,
                    unseen_escalation: None,
                }
            }
            None => self.post(forwarded).await?,
        };
        let note = NewEvent::new(
            committee,
            Kind::Note,
            Address::Speaker,
            roles_text::forwarded_note(&intended, self.locale),
        )
        .to(asker)
        .in_thread(root.seq)
        .with_meta(meta::FORWARDED_TO, serde_json::json!(intended.to_string()))
        .with_meta(meta::ACTED_BY, acted);
        let note = self.post(note).await?;
        Ok(Forwarded {
            posted,
            note,
            to: intended,
        })
    }

    /// The Senate above, when `intended` is in it: this Senate's name
    /// there and the client that writes to it. Only the parent: a seat
    /// further up is not forwarded to.
    fn above_for(&self, intended: &Locator) -> Option<(Name, &ApiClient)> {
        let client = self.uplink.as_ref()?;
        let here = self.senate_path()?;
        let parent = here.parent()?.absolute();
        let theirs = intended.senate()?;
        let in_parent =
            parent.ends_with(theirs) && theirs.clone().absolute() != here.clone().absolute();
        in_parent.then(|| (here.name().clone(), client))
    }

    /// Send a desk item back to its sender with the reason: an answer in
    /// the asker's thread.
    pub(crate) async fn on_refuse(
        &self,
        acted_by: &SenatorName,
        committee: CommitteeId,
        seq: Seq,
        reason: String,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if reason.trim().is_empty() {
            return Err(IpcError::bad_request("`reason` is empty"));
        }
        let people = self.people();
        let root = match self.docket_item(&committee, seq).await {
            Ok((item, _)) => item.event,
            Err(error) => self
                .fetch_event(&committee, seq)
                .await
                .ok()
                .filter(|event| event.is_hosted_approval(&people))
                .ok_or(error)?,
        };
        if root.is_hosted_approval(&people) {
            let mut archived = Self::status_event(
                committee,
                Address::Speaker,
                root.seq,
                &StatusPayload::new(senate_core::StatusState::Archived),
                id,
            )
            .with_meta(meta::ACTED_BY, serde_json::json!(acted_by.to_string()))
            .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(root.id));
            archived.body = reason;
            return self.post(archived).await;
        }
        let asker = root.on_behalf_of.clone().unwrap_or(root.from.clone());
        let mut event = NewEvent::new(committee, Kind::Answer, Address::Speaker, reason)
            .to(asker)
            .in_thread(root.seq)
            .with_meta(meta::ACTED_BY, serde_json::json!(acted_by.to_string()));
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// Make `senator` the chair of `committee`: for the Speaker's office
    /// (`acted_by`, a presiding session) or as the person.
    pub(crate) async fn on_set_chair(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        senator: SenatorName,
        acted_by: Option<&SenatorName>,
    ) -> Result<ChairInfo, IpcError> {
        let committee = match committee {
            Some(c) => c,
            None => {
                let session = self.resolve(selector)?;
                self.bound(&session)?
            }
        };
        let info = self
            .api
            .set_chair_as(&committee, &senator, acted_by)
            .await
            .map_err(|e| self.refusal(e))?;
        self.note_chair(&info);
        Ok(info)
    }

    /// A refusal from the server, in the person's language when it is one
    /// the bridge knows.
    fn refusal(&self, e: ApiClientError) -> IpcError {
        match e.status() {
            Some(403) => IpcError::new(
                codes::SERVER,
                roles_text::forbidden(&e.to_string(), self.locale),
            ),
            Some(400) => IpcError::new(
                codes::SERVER,
                roles_text::refused(&e.to_string(), self.locale),
            ),
            _ => server_error(e),
        }
    }

    // ---- secretaries ----------------------------------------------------------

    /// List, add or remove a senator's secretaries: the session's agent's,
    /// or without a session the person's, unless `for_` names another.
    pub(crate) async fn on_secretaries(
        &self,
        selector: &SessionSelector,
        op: SecretaryOp,
        for_: Option<SenatorName>,
        secretary: Option<SenatorName>,
    ) -> Result<SecretariesReply, IpcError> {
        let named = selector.session_id.is_some() || selector.claude_pid.is_some();
        let session = if named {
            Some(self.resolve(selector)?)
        } else {
            None
        };
        let whose = match (for_, &session) {
            (Some(s), _) => s,
            (None, Some(s)) => s.agent.clone(),
            (None, None) => SenatorName::new(self.config.person.clone())
                .map_err(|e| IpcError::bad_request(e.to_string()))?,
        };
        let all = self.api.secretaries().await.map_err(server_error)?;
        let mut list = all.get(&whose).cloned().unwrap_or_default();
        if op == SecretaryOp::List {
            return Ok(SecretariesReply {
                senator: whose,
                secretaries: list,
                all,
            });
        }
        let Some(secretary) = secretary else {
            return Err(IpcError::bad_request(roles_text::name_a_secretary(
                self.locale,
            )));
        };
        match op {
            SecretaryOp::Add if !list.contains(&secretary) => list.push(secretary),
            SecretaryOp::Remove => list.retain(|s| s != &secretary),
            _ => {}
        }
        // Writing for the Speaker's office: the desk worker acts.
        let acted_by = session
            .as_ref()
            .filter(|s| self.desk_role(&s.agent).is_some())
            .filter(|_| self.office().is_some_and(|o| o.speaker.name() == &whose))
            .map(|s| s.agent.clone());
        let all = self
            .api
            .set_secretaries(&whose, list.clone(), acted_by)
            .await
            .map_err(|e| self.refusal(e))?;
        // The desk may have changed.
        self.refresh_me().await;
        Ok(SecretariesReply {
            senator: whose.clone(),
            secretaries: all.get(&whose).cloned().unwrap_or(list),
            all,
        })
    }

    /// Register this machine's companion as a secretary of `agent`, once
    /// per daemon run, keeping the secretaries it already has.
    pub(crate) async fn register_companion(&self, agent: &SenatorName) {
        let name = self.companion_config.name.clone();
        if !self.companion_config.enabled || &name == agent {
            return;
        }
        if !lock(&self.companion_registered).insert(agent.clone()) {
            return;
        }
        let all = match self.api.secretaries().await {
            Ok(all) => all,
            Err(e) => {
                tracing::debug!(%agent, "the companion is not registered as a secretary: {e}");
                lock(&self.companion_registered).remove(agent);
                return;
            }
        };
        let mut list = all.get(agent).cloned().unwrap_or_default();
        if list.contains(&name) {
            return;
        }
        list.push(name.clone());
        match self.api.set_secretaries(agent, list, None).await {
            Ok(_) => {
                tracing::info!(%agent, secretary = %name, "the companion is a secretary of this agent")
            }
            Err(e) => {
                tracing::warn!(%agent, "registering the companion as a secretary: {e}");
                lock(&self.companion_registered).remove(agent);
            }
        }
    }

    // ---- what an agent writes -----------------------------------------------

    /// An agent writes: to addresses on its own committee, or to a
    /// qualified name. A name on another committee (or in another Senate)
    /// is written on its own committee to the Speaker's desk with
    /// `meta.intended`: the boundary rule.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn on_send(
        &self,
        selector: &SessionSelector,
        kind: SayKind,
        mut to: Vec<Address>,
        at: Option<Locator>,
        body: String,
        refs: Option<Refs>,
        id: Option<EventId>,
    ) -> Result<SendReply, IpcError> {
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let mut intended = None;
        if let Some(at) = at {
            let (level, found) =
                crate::client::resolve_outward(&self.levels(), &at, Some(&committee))
                    .await
                    .map_err(server_error)?;
            match found {
                api::Resolved::Here {
                    committee: there,
                    address,
                    ..
                } if level == 0 && there == committee => {
                    to = vec![address];
                }
                api::Resolved::Here { locator, .. } => intended = Some(locator),
                api::Resolved::Elsewhere => intended = Some(at),
                other => return Err(self.unresolved(&at, other)),
            }
        }
        if intended.is_some() {
            to = vec![Address::Speaker];
        } else if kind != SayKind::Note {
            routing::check_ask_recipients(&to, &self.people(), self.locale)
                .map_err(IpcError::bad_request)?;
        }
        let me = Address::senator(&session.agent);
        if to.contains(&me) {
            return Err(IpcError::bad_request(roles_text::not_yourself(
                &me,
                self.locale,
            )));
        }
        if to.is_empty() {
            return Err(IpcError::bad_request("`to` is empty"));
        }
        let mut refs = refs.unwrap_or_default();
        if refs.branch.is_none() {
            refs.branch = session.branch.clone();
        }
        let mut event = NewEvent::new(committee, kind.event_kind(), me, body)
            .with_to(to)
            .with_refs(refs);
        if let Some(intended) = &intended {
            event = event.with_meta(meta::INTENDED, serde_json::json!(intended.to_string()));
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        let posted = self.post(event).await?;
        let desk = posted.to.contains(&Address::Speaker);
        Ok(SendReply {
            speaker: desk.then(|| self.office().map(|o| o.speaker)).flatten(),
            senate: desk.then(|| self.senate_path()).flatten(),
            posted,
            intended,
        })
    }

    /// A qualified name that resolved to no one, or to several.
    pub(crate) fn unresolved(&self, at: &Locator, found: api::Resolved) -> IpcError {
        match found {
            api::Resolved::Unknown { reason } => IpcError::bad_request(match self.locale {
                Locale::Zh => format!("{} 无法送达：{reason}", at.in_text()),
                Locale::En => format!("{} cannot be reached: {reason}", at.in_text()),
            }),
            api::Resolved::Ambiguous { candidates } => {
                let list = candidates
                    .iter()
                    .map(Locator::in_text)
                    .collect::<Vec<_>>()
                    .join(", ");
                IpcError::bad_request(match self.locale {
                    Locale::Zh => format!("{} 指向不止一处：{list}。写明其中一个。", at.in_text()),
                    Locale::En => format!(
                        "{} names more than one place: {list}. Name one of them.",
                        at.in_text()
                    ),
                })
            }
            _ => IpcError::bad_request(match self.locale {
                Locale::Zh => format!(
                    "{} 不在这个 Senate，也不在它知道的任何上一级 Senate 里。",
                    at.in_text()
                ),
                Locale::En => format!(
                    "{} is not in this Senate, nor in any Senate above it knows of.",
                    at.in_text()
                ),
            }),
        }
    }

    // ---- escalation one step up --------------------------------------------

    /// `escalate` with no `to`: one step up. A desk worker hands it to the
    /// Speaker (a principal; an agent Speaker to its own person: it stops
    /// at the Speaker); a member to its committee's chair (a person chair
    /// gets an escalation, an agent chair a question addressed to `chair`
    /// with the thread as the origin); the chair itself, or a committee
    /// with no chair, to the Speaker's desk.
    pub(crate) async fn escalate_up(
        &self,
        session: &Session,
        committee: CommitteeId,
        root: &Event,
        reason: Option<String>,
        id: Option<EventId>,
    ) -> Result<EscalateReply, IpcError> {
        let agent = &session.agent;
        let people = self.people();
        if self.desk_role(agent).is_some() {
            let person = self
                .office()
                .and_then(|o| o.speaker_here().cloned())
                .filter(|s| people.is_person(s))
                .map(|s| s.name().clone())
                .unwrap_or_else(|| self.config.person.clone());
            return self
                .escalate_to_person(session, committee, root, person, reason, id)
                .await;
        }
        let chair = self.current_chair(&committee).await;
        let up = match &chair {
            Some(c) if c != agent && people.is_person(c) => {
                return self
                    .escalate_to_person(session, committee, root, c.name().clone(), reason, id)
                    .await;
            }
            Some(c) if c != agent => (Address::Chair, EscalatedTo::Chair { chair: c.clone() }),
            _ => (Address::Speaker, EscalatedTo::Desk),
        };
        let reason = reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| roles_text::default_escalation(root.seq, self.locale));
        let origin = Origin {
            locator: self.locator_of(&committee, Address::senator(agent)),
            thread: root.seq,
        };
        let mut event = NewEvent::new(committee, Kind::Question, Address::senator(agent), reason)
            .to(up.0)
            .with_meta(meta::ORIGIN, origin.value());
        if let Some(id) = id {
            event = event.with_id(id);
        }
        let posted = self.post(event).await?;
        Ok(EscalateReply {
            posted,
            went: Some(up.1),
        })
    }

    /// The thread is suspended on `person` (`escalated_to`).
    pub(crate) async fn escalate_to_person(
        &self,
        session: &Session,
        committee: CommitteeId,
        root: &Event,
        person: Name,
        reason: Option<String>,
        id: Option<EventId>,
    ) -> Result<EscalateReply, IpcError> {
        let mut status = StatusPayload::new(senate_core::StatusState::Suspended);
        status.escalated_to = Some(person.clone());
        status.reason = reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        status.check().map_err(IpcError::bad_request)?;
        let event = Self::status_event(
            committee,
            Address::senator(&session.agent),
            root.seq,
            &status,
            id,
        );
        let posted = self.post(event).await?;
        Ok(EscalateReply {
            posted,
            went: Some(EscalatedTo::Person { person }),
        })
    }

    // ---- what `senate status` shows ----------------------------------------------

    pub(crate) async fn on_roles(&self) -> Result<RolesReply, IpcError> {
        let me = tokio::time::timeout(ROLE_TIMEOUT, self.api.me()).await;
        let committees = match me {
            Ok(Ok(me)) => {
                self.note_senate(&me);
                self.note_office(&me);
                self.note_speaker(me.speaker.clone());
                me.committees
            }
            _ => lock(&self.chairs).keys().cloned().collect(),
        };
        let mut chairs = Vec::new();
        for committee in committees {
            match tokio::time::timeout(ROLE_TIMEOUT, self.api.chair(&committee)).await {
                Ok(Ok(info)) => {
                    self.note_chair(&info);
                    chairs.push(info);
                }
                _ => chairs.push(ChairInfo {
                    chair: self.chair_of(&committee),
                    committee,
                    source: None,
                }),
            }
        }
        Ok(RolesReply {
            chairs,
            ..self.roles_now()
        })
    }

    /// The roles as this side knows them now, with no request: the office
    /// and the children `/v1/me` last named, the chairs known from the
    /// log, and the registration with the Senate above.
    pub(crate) fn roles_now(&self) -> RolesReply {
        RolesReply {
            senate: self.senate_path(),
            office: self.office(),
            present: self.present_workers(),
            chairs: self
                .known_chairs()
                .into_iter()
                .map(|(committee, chair)| ChairInfo {
                    committee,
                    chair: Some(chair),
                    source: None,
                })
                .collect(),
            children: lock(&self.children).clone(),
            uplink: self.uplink_status(),
        }
    }

    /// What the machine's uplink last wrote, when this Senate is a child.
    pub(crate) fn uplink_status(&self) -> Option<crate::ipc::UplinkStatus> {
        let uplink = self.config.uplink.as_ref()?;
        let dir = crate::paths::Paths::new(&self.home).hosted(HOSTED_KEY);
        Some(crate::ipc::UplinkStatus::read(&dir).unwrap_or_else(|| {
            crate::ipc::UplinkStatus {
                url: uplink.url.clone(),
                parent: None,
                name: self
                    .senate_path()
                    .map(|p| p.name().clone())
                    .unwrap_or_else(|| crate::config::default_desk_name(&self.config.person)),
                speaker: self.speaker_id().unwrap_or_else(|| {
                    SenatorId::here(
                        SenatorName::new(self.config.person.clone())
                            .unwrap_or_else(|_| unreachable!("a handle is a senator name")),
                    )
                }),
                registration: crate::ipc::Registration::Pending,
                at: chrono::Utc::now(),
            }
        }))
    }

    /// The Speaker of this Senate, as a senator id, when known.
    pub(crate) fn speaker_id(&self) -> Option<SenatorId> {
        self.office().map(|o| o.speaker)
    }
}

/// What the roles' requests say, in the bridge's locale.
pub(crate) mod roles_text {
    use senate_core::{Address, CommitteeId, Locale, Locator, SenatorName, Seq};

    fn mention(s: &SenatorName) -> String {
        Address::senator(s).in_text()
    }

    fn mentions(names: &[SenatorName]) -> String {
        names.iter().map(mention).collect::<Vec<_>>().join(", ")
    }

    pub(crate) fn not_chairing(
        agent: &SenatorName,
        committee: &CommitteeId,
        chair: Option<&SenatorName>,
        locale: Locale,
    ) -> String {
        let me = mention(agent);
        let chair = chair.map(mention);
        match (locale, chair) {
            (Locale::Zh, Some(c)) => format!(
                "{me} 不是委员会 `{committee}` 的主席（chair），{c} 才是；这个会话没有主席工具。\
                 主席工具（chair_docket、chair_route、chair_cover、chair_escalate）用来处理\
                 等待主席的事项，只归主席使用；要找主席，用 ask 发给 `chair`。"
            ),
            (Locale::Zh, None) => format!(
                "{me} 不是委员会 `{committee}` 的主席（chair）；这个会话没有主席工具。\
                 主席工具（chair_docket、chair_route、chair_cover、chair_escalate）用来处理\
                 等待主席的事项，只归主席使用；要找主席，用 ask 发给 `chair`。"
            ),
            (Locale::En, Some(c)) => format!(
                "{me} does not chair the committee `{committee}`: the chair of `{committee}` is \
                 {c}, so this session has no chair tools. The chair tools (chair_docket, \
                 chair_route, chair_cover, chair_escalate) work what awaits the chair and are the \
                 chair's alone; to reach the chair, ask `chair`."
            ),
            (Locale::En, None) => format!(
                "{me} does not chair the committee `{committee}`, so this session has no chair \
                 tools. The chair tools (chair_docket, chair_route, chair_cover, chair_escalate) \
                 work what awaits the chair and are the chair's alone; to reach the chair, ask \
                 `chair`."
            ),
        }
    }

    pub(crate) fn not_on_chair_docket(committee: &CommitteeId, seq: Seq, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "`{committee}` 的 seq {seq} 不在主席（chair）的待办里（可能已处理）。先用 chair_docket 看看。"
            ),
            Locale::En => format!(
                "seq {seq} in `{committee}` does not await the chair (it may be dealt with). Look \
                 with chair_docket first."
            ),
        }
    }

    pub(crate) fn not_a_candidate(
        to: &SenatorName,
        candidates: &[SenatorName],
        locale: Locale,
    ) -> String {
        let to = mention(to);
        match locale {
            Locale::Zh => format!("{to} 不是候选之一；从 {} 中选一个。", mentions(candidates)),
            Locale::En => format!(
                "{to} is not one of the candidates; pick one of {}.",
                mentions(candidates)
            ),
        }
    }

    pub(crate) fn never_reroute(locale: Locale) -> String {
        locale
            .pick(
                "这是问某个不在的成员的问题：不转给别人。用 chair_cover 依据它自己的 checkpoint 代答，\
                 或 chair_escalate 交给 Speaker 的办公桌。",
                "This is a question to an absent member: it is never rerouted to someone else. \
                 Answer for it from its own checkpoint with chair_cover, or put it to the Speaker's \
                 desk with chair_escalate.",
            )
            .to_owned()
    }

    pub(crate) fn route_to_a_member(to: &SenatorName, locale: Locale) -> String {
        let to = mention(to);
        match locale {
            Locale::Zh => format!("只能交给这个委员会的另一个 agent，不能交给 {to}。"),
            Locale::En => format!("Route it to another agent of this committee, not to {to}."),
        }
    }

    pub(crate) fn never_a_request(locale: Locale) -> String {
        locale
            .pick(
                "secretary 只代答问题，不替它去做请求：留着，它回来后会自己收到。",
                "A secretary answers questions for its principal and never performs a request: \
                 leave it, the agent gets it itself when it returns.",
            )
            .to_owned()
    }

    pub(crate) fn not_yours_to_cover(principal: &SenatorName, locale: Locale) -> String {
        let principal = mention(principal);
        match locale {
            Locale::Zh => format!(
                "这个问题现在不归你代 {principal} 回答：它已经回来了，或已有人回答了，\
                 或你不是它的第一个在线的 secretary。"
            ),
            Locale::En => format!(
                "This question is not yours to answer for {principal} now: it is back, or \
                 someone answered, or you are not its first present secretary."
            ),
        }
    }

    pub(crate) fn no_checkpoint_to_cover(principal: &SenatorName, locale: Locale) -> String {
        let principal = mention(principal);
        match locale {
            Locale::Zh => format!(
                "{principal} 在这个委员会没有 checkpoint，没有依据可以代它回答：留着，它回来后会自己收到。"
            ),
            Locale::En => format!(
                "{principal} has no checkpoint on this committee, so there is nothing to answer \
                 for it from: leave it, it gets it itself when it returns."
            ),
        }
    }

    pub(crate) fn not_away(locale: Locale) -> String {
        locale
            .pick(
                "这件事不是问某个不在的成员的问题，不能代答。用 chair_route 或 chair_escalate。",
                "This item is not a question to an absent member, so there is no one to answer \
                 for. Use chair_route or chair_escalate.",
            )
            .to_owned()
    }

    pub(crate) fn not_its_question(
        agent: &SenatorName,
        agents: &[SenatorName],
        locale: Locale,
    ) -> String {
        let agent = mention(agent);
        match locale {
            Locale::Zh => format!(
                "这个问题不是问 {agent} 的；只能代 {} 回答。",
                mentions(agents)
            ),
            Locale::En => format!(
                "The question was not addressed to {agent}; answer only for {}.",
                mentions(agents)
            ),
        }
    }

    pub(crate) fn name_which(agents: &[SenatorName], locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "问题问的是几个成员：用 `agent` 指定代谁回答：{}。",
                mentions(agents)
            ),
            Locale::En => format!(
                "The question went to several members: name the one you answer for with `agent`: {}.",
                mentions(agents)
            ),
        }
    }

    pub(crate) fn no_checkpoint(agent: &SenatorName, locale: Locale) -> String {
        let agent = mention(agent);
        match locale {
            Locale::Zh => format!(
                "{agent} 在这个委员会没有 checkpoint，没有依据可以代它回答。用 chair_escalate 交给 Speaker 的办公桌。"
            ),
            Locale::En => format!(
                "{agent} has no checkpoint on this committee, so there is nothing to answer for it \
                 from. Put it to the Speaker's desk with chair_escalate."
            ),
        }
    }

    /// The first line of a chair's answer for an absent member.
    pub(crate) fn covering_line(member: &SenatorName, checkpoint: Seq, locale: Locale) -> String {
        let member = mention(member);
        match locale {
            Locale::Zh => format!("（主席代 {member} 回答，依据它的 checkpoint seq {checkpoint}）"),
            Locale::En => {
                format!("(the chair answering for {member}, from its checkpoint seq {checkpoint})")
            }
        }
    }

    pub(crate) fn nothing_intended(locale: Locale) -> String {
        locale
            .pick(
                "这件事没有写明要送去哪里（没有 meta.intended）：用 speaker_route 交给这里的 agent，\
                 或 speaker_escalate。",
                "This item names no intended recipient (no meta.intended): hand it to an agent \
                 here with speaker_route, or use speaker_escalate.",
            )
            .to_owned()
    }

    pub(crate) fn not_in_this_senate(to: &Locator, locale: Locale) -> String {
        let to = to.in_text();
        match locale {
            Locale::Zh => format!(
                "{to} 在另一个 Senate：这里只能转给本 Senate 的席位。把它留给 Speaker，或用 speaker_refuse 说明理由退回。"
            ),
            Locale::En => format!(
                "{to} is in another Senate: only a seat of this Senate can be forwarded to from \
                 here. Leave it for the Speaker, or send it back with a reason with speaker_refuse."
            ),
        }
    }

    /// The desk's note in the asker's thread.
    pub(crate) fn forwarded_note(to: &Locator, locale: Locale) -> String {
        let to = to.in_text();
        match locale {
            Locale::Zh => format!("Speaker 办公室已把它转给 {to}；回答会发回这个 thread。"),
            Locale::En => format!(
                "The Speaker's office forwarded this to {to}; the answer comes back to this thread."
            ),
        }
    }

    pub(crate) fn forbidden(detail: &str, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "Senate 拒绝了：只有这个 senator 自己、运行它的 principal 或 Speaker 办公室可以这样做。（{detail}）"
            ),
            Locale::En => format!(
                "The Senate refused: only that senator itself, the principal that runs it, or the \
                 Speaker's office may do this. ({detail})"
            ),
        }
    }

    pub(crate) fn refused(detail: &str, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!("Senate 拒绝了这个请求：{detail}"),
            Locale::En => format!("The Senate refused this: {detail}"),
        }
    }

    pub(crate) fn name_a_secretary(locale: Locale) -> String {
        locale
            .pick(
                "说明要加入或移除哪个 secretary（一个 agent 的名字）。",
                "Name the secretary to add or remove (an agent's name).",
            )
            .to_owned()
    }

    pub(crate) fn not_yourself(me: &Address, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!("{} 就是这个会话自己；请写给别人", me.in_text()),
            Locale::En => format!(
                "{} is this session's own agent; write to someone else",
                me.in_text()
            ),
        }
    }

    pub(crate) fn default_escalation(thread: Seq, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!("thread {thread} 需要上一级处理。"),
            Locale::En => format!("Thread {thread} needs someone one step up."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::roles_text;
    use senate_core::{CommitteeId, Locale, SenatorName};

    #[test]
    fn a_non_chair_is_told_who_chairs_in_both_languages() {
        let me: SenatorName = "codex".parse().unwrap();
        let chair: SenatorName = "claude".parse().unwrap();
        let c = CommitteeId::new("ibot").unwrap();
        let en = roles_text::not_chairing(&me, &c, Some(&chair), Locale::En);
        assert!(en.contains("[senator::codex] does not chair the committee `ibot`"));
        assert!(
            en.contains("the chair of `ibot` is [senator::claude]"),
            "{en}"
        );
        assert!(en.contains("chair_docket"), "{en}");
        let zh = roles_text::not_chairing(&me, &c, None, Locale::Zh);
        assert!(zh.starts_with("[senator::codex] 不是委员会 `ibot` 的主席"));
    }
}

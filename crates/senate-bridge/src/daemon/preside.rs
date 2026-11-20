//! The presiding session's requests, served by the Senate whose Speaker's
//! seat its agent holds ([`crate::preside`] says what they are for). Each
//! writes as `speaker` with this Senate's token; the server checks that the
//! person holds the seat, and this side checks that the item is on the
//! docket and that the request fits it.

use senate_core::{Awaiting, DocketItem, DocketPage, Refs};

use super::*;
use crate::ipc::SenateDocket;

/// Bound on reading the docket while a hook waits.
const HOOK_DOCKET_TIMEOUT: Duration = Duration::from_secs(1);

impl Shared {
    /// The agents seated in this Senate that this machine knows of: every
    /// agent with a session here on record.
    pub(crate) fn seated(&self) -> Vec<SenatorName> {
        self.served_agents()
            .map(|agents| agents.into_iter().collect())
            .unwrap_or_default()
    }

    /// The committees of this Senate a docket covers: `committee`, or every
    /// committee its log has.
    async fn docket_committees(
        &self,
        committee: Option<CommitteeId>,
    ) -> Result<Vec<CommitteeId>, IpcError> {
        if let Some(committee) = committee {
            return Ok(vec![committee]);
        }
        let me = self.api.me().await.map_err(server_error)?;
        self.note_office(&me);
        self.note_speaker(me.speaker.clone());
        Ok(me.committees)
    }

    /// What awaits this Senate's Speaker, with the latest checkpoint of
    /// every away agent an item names.
    pub(crate) async fn docket(
        &self,
        committee: Option<CommitteeId>,
    ) -> Result<SenateDocket, IpcError> {
        let mut pages = Vec::new();
        let mut checkpoints = Vec::new();
        for committee in self.docket_committees(committee).await? {
            let page = self.api.docket(&committee).await.map_err(server_error)?;
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
            pages.push(page);
        }
        Ok(SenateDocket {
            senate: self.senate_name(),
            pages,
            checkpoints,
        })
    }

    /// One item on the committee's docket, and the page it is on.
    pub(crate) async fn docket_item(
        &self,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<(DocketItem, DocketPage), IpcError> {
        let page = self.api.docket(committee).await.map_err(server_error)?;
        let item = page
            .items
            .iter()
            .find(|i| i.event.seq == seq)
            .cloned()
            .ok_or_else(|| {
                IpcError::bad_request(preside_text::not_on_docket(committee, seq, self.locale))
            })?;
        Ok((item, page))
    }

    /// Relay an item to one agent seated here: a pick to one of its
    /// candidates, or a message addressed to the Speaker to whichever agent
    /// should have it. A question to an away agent is never rerouted.
    pub(crate) async fn on_route(
        &self,
        speaker: &SenatorName,
        committee: CommitteeId,
        seq: Seq,
        to: SenatorName,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let (item, _) = self.docket_item(&committee, seq).await?;
        match &item.awaiting {
            Awaiting::Pick { candidates, .. } if !candidates.contains(&to) => {
                return Err(IpcError::bad_request(preside_text::not_a_candidate(
                    &to,
                    candidates,
                    self.locale,
                )));
            }
            Awaiting::Away { .. } => {
                return Err(IpcError::bad_request(preside_text::never_reroute(
                    self.locale,
                )));
            }
            Awaiting::ChildOffline { child } => {
                return Err(IpcError::bad_request(preside_text::for_an_offline_child(
                    child,
                    self.locale,
                )));
            }
            _ if self.people().is_person(&to) || &to == speaker => {
                return Err(IpcError::bad_request(preside_text::route_to_an_agent(
                    &to,
                    self.locale,
                )));
            }
            _ => {}
        }
        let root = item.event;
        let behalf = root
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| root.from.clone());
        let mut event = NewEvent::new(committee, root.kind.clone(), Address::Speaker, root.body)
            .to(Address::senator(&to))
            .on_behalf_of(behalf)
            .with_meta(meta::RELAYED_FROM, serde_json::json!(seq.0))
            .with_meta(meta::ACTED_BY, serde_json::json!(speaker.to_string()));
        if let Some(refs) = root.refs {
            event = event.with_refs(refs);
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// Answer a question to an away agent on that agent's behalf, from its
    /// own latest checkpoint: labelled `[speaker::<agent>]` with the
    /// takeover word, pointing at the checkpoint's branch and commit.
    pub(crate) async fn on_cover(
        &self,
        speaker: &SenatorName,
        committee: CommitteeId,
        thread: Seq,
        body: String,
        agent: Option<SenatorName>,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        if agent.as_ref() == Some(speaker) {
            return Err(IpcError::bad_request(preside_text::cover_for_the_speaker(
                speaker,
                self.locale,
            )));
        }
        let (item, _) = self.docket_item(&committee, thread).await?;
        if let Awaiting::ChildOffline { child } = &item.awaiting {
            let body = senate_core::docket::child_offline_answer(self.locale, child, &body);
            return self
                .say_of_offline_child(speaker, item.event, child, Kind::Answer, body, id)
                .await;
        }
        let Awaiting::Away { agents } = &item.awaiting else {
            return Err(IpcError::bad_request(preside_text::not_away(self.locale)));
        };
        let agent = match (agent, agents.as_slice()) {
            (Some(agent), _) if agents.contains(&agent) => agent,
            (Some(agent), _) => {
                return Err(IpcError::bad_request(preside_text::not_its_question(
                    &agent,
                    agents,
                    self.locale,
                )));
            }
            (None, [one]) => one.clone(),
            (None, several) => {
                return Err(IpcError::bad_request(preside_text::name_which(
                    several,
                    self.locale,
                )));
            }
        };
        let Some(checkpoint) = self
            .api
            .latest_checkpoint(&committee, &agent)
            .await
            .map_err(server_error)?
        else {
            return Err(IpcError::bad_request(preside_text::no_checkpoint(
                &agent,
                self.locale,
            )));
        };
        let root = item.event;
        let asker = root
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| root.from.clone());
        let mut event = NewEvent::new(committee, Kind::Answer, Address::Speaker, body)
            .to(asker)
            .in_thread(root.seq)
            .on_behalf_of(Address::senator(&agent))
            .with_refs(Refs {
                commit: Some(checkpoint.checkpoint.commit.clone()),
                path: None,
                branch: Some(checkpoint.checkpoint.branch.clone()),
            })
            .with_meta(meta::TAKEOVER, serde_json::json!(true))
            .with_meta(meta::ACTED_BY, serde_json::json!(speaker.to_string()));
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// Tell the one who asked a child Senate that is offline that their
    /// question is held for it.
    pub(crate) async fn on_hold(
        &self,
        speaker: &SenatorName,
        committee: CommitteeId,
        seq: Seq,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let (item, _) = self.docket_item(&committee, seq).await?;
        let Awaiting::ChildOffline { child } = &item.awaiting else {
            return Err(IpcError::bad_request(
                preside_text::not_for_an_offline_child(self.locale),
            ));
        };
        // As the Senate says now: this side's copy is from its last `/v1/me`.
        let seen = self
            .api
            .children()
            .await
            .map_err(server_error)?
            .into_iter()
            .find(|c| c.name == *child)
            .and_then(|c| c.last_seen);
        let away = seen.map_or_else(chrono::Duration::zero, |seen| chrono::Utc::now() - seen);
        let body = senate_core::docket::child_offline_held(self.locale, child, away);
        self.say_of_offline_child(speaker, item.event, child, Kind::Note, body, id)
            .await
    }

    /// The office's line in the thread of `root`, a question for the child
    /// Senate `child` that is offline: an `answer` on the child's behalf,
    /// or the `note` that it is held. To the one who asked, and an answer
    /// to the child's desk as well; the question itself still reaches the
    /// child when it reconnects.
    async fn say_of_offline_child(
        &self,
        speaker: &SenatorName,
        root: Event,
        child: &Name,
        kind: Kind,
        body: String,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let asker = root
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| root.from.clone());
        let desk = root
            .to
            .iter()
            .find(|a| matches!(a, Address::Child { senate, .. } if senate == child))
            .cloned();
        let answered = kind == Kind::Answer;
        let mut event = NewEvent::new(root.committee.clone(), kind, Address::Speaker, body)
            .in_thread(root.seq)
            .with_meta(meta::CHILD_OFFLINE, serde_json::json!(child.as_str()))
            .with_meta(meta::ACTED_BY, serde_json::json!(speaker.to_string()));
        if asker != Address::Speaker {
            event = event.to(asker);
        }
        if let (true, Some(desk)) = (answered, desk) {
            // To the child's desk too: it finds the answer under the
            // question when it reconnects.
            event = event.on_behalf_of(desk.clone()).to(desk);
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// Put an item to the person: a task thread is suspended on them
    /// (`escalated_to`); anything else gets a question to them in its
    /// thread. The person is the one whose agent was meant (a pick, an away
    /// agent), else the Speaker's own person.
    pub(crate) async fn on_refer(
        &self,
        speaker: &SenatorName,
        committee: CommitteeId,
        seq: Seq,
        reason: String,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        if reason.trim().is_empty() {
            return Err(IpcError::bad_request("`reason` is empty"));
        }
        let (item, _) = self.docket_item(&committee, seq).await?;
        // The person whose agent was meant, else the Speaker when it is a
        // person, else the person who runs this machine's agents.
        let people = self.people();
        let person = match &item.awaiting {
            Awaiting::Pick {
                owner: Some(owner), ..
            } => owner.clone(),
            _ => self
                .office()
                .and_then(|o| o.speaker_here().cloned())
                .filter(|s| people.is_person(s))
                .map(|s| s.name().clone())
                .unwrap_or_else(|| self.config.person.clone()),
        };
        let acted = serde_json::json!(speaker.to_string());
        let root = item.event;
        let task = root.thread.is_none()
            && matches!(root.kind, Kind::Question | Kind::Request)
            && root.to.iter().any(|a| people.is_agent_address(a));
        let mut event = if task {
            let mut status = StatusPayload::new(senate_core::StatusState::Suspended);
            status.escalated_to = Some(person);
            status.reason = Some(reason.clone());
            NewEvent::new(committee, Kind::Status, Address::Speaker, reason)
                .in_thread(root.seq)
                .with_meta(meta::STATUS, status.value())
                .with_meta(meta::ACTED_BY, acted)
        } else {
            NewEvent::new(committee, Kind::Question, Address::Speaker, reason)
                .to(Address::senior(&person))
                .in_thread(root.seq)
                .with_meta(meta::ACTED_BY, acted)
        };
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// What a presiding session is told by a hook: at session start that it
    /// presides, over which Senate, who sits there and how much awaits it;
    /// at a prompt, only items it has not been told of. `None` when there is
    /// nothing to say or the docket is not read in time.
    pub(crate) async fn presiding_context(
        &self,
        session: &Session,
        event: HookEvent,
    ) -> Option<String> {
        let docket = match tokio::time::timeout(HOOK_DOCKET_TIMEOUT, self.docket(None)).await {
            Ok(Ok(docket)) => Some(docket),
            Ok(Err(e)) => {
                tracing::debug!("presiding without the docket: {e}");
                None
            }
            Err(_) => None,
        };
        let items: BTreeSet<(CommitteeId, Seq)> = docket
            .iter()
            .flat_map(|d| &d.pages)
            .flat_map(|p| p.items.iter().map(|i| (p.committee.clone(), i.event.seq)))
            .collect();
        let new = {
            let mut announced = lock(&self.announced);
            let told = announced.entry(session.session_id.clone()).or_default();
            let new = items.difference(told).count();
            told.extend(items.iter().cloned());
            new
        };
        let senate = self.senate_path();
        match event {
            HookEvent::SessionStart => {
                let mut seated: BTreeSet<SenatorName> = self.seated().into_iter().collect();
                if let Some(docket) = &docket {
                    for page in &docket.pages {
                        seated.extend(page.seats.iter().map(|s| s.agent.clone()));
                    }
                }
                let seated: Vec<SenatorName> = seated.into_iter().collect();
                Some(crate::preside::orientation_as(
                    self.desk_role(&session.agent).unwrap_or_default(),
                    self.speaker_id().as_ref(),
                    &session.agent,
                    senate.as_ref(),
                    &seated,
                    items.len(),
                    self.locale,
                ))
            }
            HookEvent::UserPromptSubmit if new > 0 => {
                Some(crate::preside::nudge(new, senate.as_ref(), self.locale))
            }
            HookEvent::UserPromptSubmit => None,
        }
    }
}

/// Refusals of the presiding requests, in the bridge's locale.
mod preside_text {
    use senate_core::{Address, CommitteeId, Locale, SenatorName, Seq};

    fn mentions(paths: &[SenatorName]) -> String {
        paths
            .iter()
            .map(|p| Address::senator(p).in_text())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(super) fn not_on_docket(committee: &CommitteeId, seq: Seq, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "`{committee}` 的 seq {seq} 不在 Speaker 的待办里（可能已处理）。先用 speaker_docket 看看。"
            ),
            Locale::En => format!(
                "seq {seq} in `{committee}` does not await the Speaker (it may be dealt with). \
                 Look with speaker_docket first."
            ),
        }
    }

    pub(super) fn not_a_candidate(
        to: &SenatorName,
        candidates: &[SenatorName],
        locale: Locale,
    ) -> String {
        let to = Address::senator(to).in_text();
        match locale {
            Locale::Zh => format!("{to} 不是候选之一；从 {} 中选一个。", mentions(candidates)),
            Locale::En => format!(
                "{to} is not one of the candidates; pick one of {}.",
                mentions(candidates)
            ),
        }
    }

    pub(super) fn never_reroute(locale: Locale) -> String {
        locale
            .pick(
                "这是问某个不在的 agent 的问题：不转给它的兄弟 agent。用 speaker_cover 依据它自己的 \
                 checkpoint 代答，或 speaker_escalate 交给人。",
                "This is a question to an away agent: it is never rerouted to a sibling. Answer \
                 for it from its own checkpoint with speaker_cover, or put it to the person with \
                 speaker_escalate.",
            )
            .to_owned()
    }

    pub(super) fn route_to_an_agent(to: &SenatorName, locale: Locale) -> String {
        let to = Address::senator(to).in_text();
        match locale {
            Locale::Zh => format!("只能交给这个 Senate 里在席的另一个 agent，不能交给 {to}。"),
            Locale::En => {
                format!("Route it to another agent seated in this Senate, not to {to}.")
            }
        }
    }

    pub(super) fn cover_for_the_speaker(speaker: &SenatorName, locale: Locale) -> String {
        let speaker = Address::senator(speaker).in_text();
        match locale {
            Locale::Zh => format!(
                "{speaker} 自己就是这个 Senate 的 Speaker，不能代它回答：写给它的问题等它的会话打开后由它自己回答。"
            ),
            Locale::En => format!(
                "{speaker} holds the Speaker's seat of this Senate, so there is no one to answer \
                 for: a question to it waits for its own session."
            ),
        }
    }

    pub(super) fn for_an_offline_child(child: &senate_core::Name, locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "这是问子 Senate [{child}] 的问题，它目前离线，不能转给这里的 agent。用 \
                 speaker_cover 代答，或 speaker_hold 告诉提问者已保留。"
            ),
            Locale::En => format!(
                "This is a question for the child Senate [{child}], which is offline; it is not \
                 rerouted to an agent here. Answer on its behalf with speaker_cover, or tell the \
                 asker it is held with speaker_hold."
            ),
        }
    }

    pub(super) fn not_for_an_offline_child(locale: Locale) -> String {
        locale
            .pick(
                "这件事不是问某个离线子 Senate 的问题，没有什么可保留的。",
                "This item is not a question for a child Senate that is offline, so there is \
                 nothing to hold.",
            )
            .to_owned()
    }

    pub(super) fn not_away(locale: Locale) -> String {
        locale
            .pick(
                "这件事不是问某个不在的 agent 的问题，不能代答。用 speaker_route 或 speaker_escalate。",
                "This item is not a question to an away agent, so there is no one to answer for. \
                 Use speaker_route or speaker_escalate.",
            )
            .to_owned()
    }

    pub(super) fn not_its_question(
        agent: &SenatorName,
        agents: &[SenatorName],
        locale: Locale,
    ) -> String {
        let agent = Address::senator(agent).in_text();
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

    pub(super) fn name_which(agents: &[SenatorName], locale: Locale) -> String {
        match locale {
            Locale::Zh => format!(
                "问题问的是几个 agent：用 `agent` 指定代谁回答：{}。",
                mentions(agents)
            ),
            Locale::En => format!(
                "The question went to several agents: name the one you answer for with `agent`: {}.",
                mentions(agents)
            ),
        }
    }

    pub(super) fn no_checkpoint(agent: &SenatorName, locale: Locale) -> String {
        let agent = Address::senator(agent).in_text();
        match locale {
            Locale::Zh => format!(
                "{agent} 在这个委员会没有 checkpoint，没有依据可以代它回答。用 speaker_escalate 交给人。"
            ),
            Locale::En => format!(
                "{agent} has no checkpoint on this committee, so there is nothing to answer for it \
                 from. Put it to the person with speaker_escalate."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::preside_text;
    use senate_core::{Locale, SenatorName};

    #[test]
    fn covering_for_the_seat_holder_is_refused_in_both_languages() {
        let holder: SenatorName = "claude".parse().unwrap();
        let zh = preside_text::cover_for_the_speaker(&holder, Locale::Zh);
        let en = preside_text::cover_for_the_speaker(&holder, Locale::En);
        assert!(zh.starts_with("[senator::claude] 自己就是这个 Senate 的 Speaker"));
        assert!(en.starts_with("[senator::claude] holds the Speaker's seat"));
    }
}

//! The Agent group: created on a committee's first binding where the
//! surface can, adopted where it cannot, archived when the last people group
//! goes; inviting people into it and its output level.
//!
//! People are invited by their accounts on this surface's platform, as the
//! roster lists them ([`Senior::account_ids`](senate_core::Senior::account_ids));
//! a person is invited once however many of their agents show up.

use senate_core::{Mention, Name, NewGroup, OutputLevel};

use super::{
    Address, BindingAction, Engine, Event, Flow, GroupRef, GroupRole, Kind, MirrorPlan, NewEvent,
    Outcome, PostTarget, Stall, debug, derived_event_id, group_binding, json, meta, warn,
};
use crate::render;

/// The [`MirrorRef`](senate_core::MirrorRef) target of the one "no Agent
/// group yet" hint, on the people group's binding.
fn hint_target(group: &str) -> String {
    format!("{group}#agents-hint")
}

impl Engine {
    /// A `binding` from the Speaker, already folded into the committee's
    /// state: the Agent group's lifecycle hangs on these.
    pub(super) async fn on_binding(&mut self, event: &Event) -> Result<(), Stall> {
        if event.from != Address::Speaker {
            return Ok(());
        }
        let Some(binding) = event
            .binding()
            .filter(|b| b.platform == self.surface.platform())
        else {
            return Ok(());
        };
        let Some(state) = self.committees.get(&event.committee) else {
            return Ok(());
        };
        let has_agents = state.agents_group().is_some();
        let people = state.people_groups().count();
        match (binding.action, binding.role) {
            (BindingAction::Bind, GroupRole::People) if !has_agents && people == 1 => {
                self.create_agents_group(event, &binding.group_id).await
            }
            (BindingAction::Bind, GroupRole::Agents) => {
                let current = state
                    .agents_group()
                    .is_some_and(|g| g.group.group_id == binding.group_id);
                if current {
                    self.post_pinned_cards(&event.committee).await?;
                }
                Ok(())
            }
            (BindingAction::Unbind, GroupRole::People) if people == 0 => {
                self.archive_agents_group(event).await
            }
            _ => Ok(()),
        }
    }

    /// The committee's first people group is bound and it has no Agent group:
    /// create `[Agents] <committee>` with the binder and every person one of
    /// whose agents is connected, and bind it. Where the surface cannot
    /// create groups, say once in the people group how to adopt one instead.
    async fn create_agents_group(&mut self, binding: &Event, people: &str) -> Result<(), Stall> {
        let l = self.locale(&binding.committee);
        let committee = binding.committee.clone();
        let name = render::agents_group_name(l, &committee);
        let mut invite: Vec<String> = Vec::new();
        let mut add = |id: Option<&String>| {
            if let Some(id) = id
                && !invite.contains(id)
            {
                invite.push(id.clone());
            }
        };
        let platform = self.surface.platform().to_owned();
        let account = |person: &Name| {
            self.roster
                .senior(person)
                .and_then(|s| s.account_ids(&platform).next())
                .map(str::to_owned)
        };
        if let Some(binder) = binding
            .on_behalf_of
            .as_ref()
            .and_then(|a| self.person_of(a))
        {
            add(account(&binder).as_ref());
        }
        for agent in self.roster.agents() {
            if self.hub.is_agent_connected(&committee, agent).await {
                add(account(agent.owner()).as_ref());
            }
        }
        let operation = self.op_id(&committee, binding.seq, "agents-group")?;
        let group = NewGroup {
            name: name.clone(),
            invite,
        };
        match self
            .call(self.surface.create_group(&group, &operation))
            .await?
        {
            Outcome::Done(created) => {
                let group = GroupRef {
                    platform: self.surface.platform().to_owned(),
                    group_id: created.group_id,
                    group_name: Some(name.clone()),
                };
                let id = derived_event_id(&[
                    "agents-group",
                    committee.as_str(),
                    &binding.seq.to_string(),
                ]);
                self.append(group_binding(
                    l,
                    &committee,
                    &group,
                    BindingAction::Bind,
                    GroupRole::Agents,
                    binding.on_behalf_of.as_ref(),
                    id,
                ))
                .await?;
                if created.not_added.is_empty() {
                    return Ok(());
                }
                // The group exists but people are missing from it (an app
                // that may create chats but not add members): say so where
                // they are, once.
                let names: Vec<String> = created
                    .not_added
                    .iter()
                    .map(|id| {
                        self.roster
                            .by_account(&platform, id)
                            .map(|s| Address::senior(&s.handle).in_text())
                            .unwrap_or_else(|| id.clone())
                    })
                    .collect();
                let plan = MirrorPlan {
                    to: PostTarget::NewRoot {
                        group_id: people.to_owned(),
                    },
                    text: render::agents_group_members_missing(l, &name, &names),
                    target: format!("{people}#agents-members-missing"),
                    record_as: None,
                    card: None,
                };
                self.run_plan(&committee, binding.seq, plan).await
            }
            Outcome::Unsupported => {
                if !binding
                    .on_behalf_of
                    .as_ref()
                    .is_some_and(Address::is_senior)
                {
                    return Ok(()); // nobody to tell
                }
                let plan = MirrorPlan {
                    to: PostTarget::NewRoot {
                        group_id: people.to_owned(),
                    },
                    text: render::no_agents_group_hint(l, &committee, &self.config.bot_name),
                    target: hint_target(people),
                    record_as: None,
                    card: None,
                };
                self.run_plan(&committee, binding.seq, plan).await
            }
            Outcome::Refused => Ok(()),
        }
    }

    /// The last people group went: rename the Agent group
    /// `[Archived] <committee>` (where the surface can) and unbind it, which
    /// is what archiving it means in the log.
    async fn archive_agents_group(&mut self, unbind: &Event) -> Result<(), Stall> {
        let l = self.locale(&unbind.committee);
        let Some(agents) = self
            .committees
            .get(&unbind.committee)
            .and_then(super::CommitteeState::agents_group)
            .map(|g| g.group.clone())
        else {
            return Ok(());
        };
        let committee = unbind.committee.clone();
        let operation = self.op_id(
            &committee,
            unbind.seq,
            &format!("{}#archive", agents.group_id),
        )?;
        let renamed = self
            .call(self.surface.rename_group(
                &agents.group_id,
                &render::archived_group_name(l, &committee),
                &operation,
            ))
            .await?;
        if !matches!(renamed, Outcome::Done(())) {
            debug!(%committee, "the Agent group is archived without a rename");
        }
        let id = derived_event_id(&["archive", committee.as_str(), &unbind.seq.to_string()]);
        self.append(group_binding(
            l,
            &committee,
            &agents,
            BindingAction::Unbind,
            GroupRole::Agents,
            unbind.on_behalf_of.as_ref(),
            id,
        ))
        .await
        .map(drop)
    }

    /// 「把 @alice 拉进 agent 群」 / "add @alice to the agent group": the
    /// people named, and every person the chat mentions, invited into the
    /// committee's Agent group. Naming an agent (`alice/claude`) invites its
    /// person.
    pub(super) async fn invite_to_agents(
        &mut self,
        chat: &Event,
        who: &[String],
    ) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        let committee = self.acting_committee(chat);
        let Some(agents) = self
            .committees
            .get(&committee)
            .and_then(super::CommitteeState::agents_group)
            .map(|g| g.group.group_id.clone())
        else {
            return self
                .say(chat, &render::no_agents_group(l, &committee))
                .await;
        };
        let mut names: Vec<String> = who.to_vec();
        if let Some(mentions) = chat.meta.get(meta::MENTIONS).and_then(|m| m.as_array()) {
            // The gateway records people it mentions by their address here;
            // a guest is nobody to invite by handle.
            names.extend(
                mentions
                    .iter()
                    .filter_map(|m| m.get("address").and_then(|a| a.as_str()))
                    .filter_map(|a| a.parse::<Address>().ok())
                    .filter_map(|a| a.as_senator().map(|p| p.owner().to_string())),
            );
        }
        let platform = self.surface.platform().to_owned();
        let mut handles = Vec::new();
        let mut ids = Vec::new();
        let mut unknown = Vec::new();
        for raw in &names {
            let raw = raw.trim().trim_start_matches('@');
            let person = match raw.parse::<Mention>() {
                Ok(Mention::Senator(path)) => path.owner().to_string(),
                _ => raw
                    .strip_prefix("senator::")
                    .unwrap_or(raw)
                    .split('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
            };
            match self.roster.resolve(&person) {
                Some(senior) => {
                    let handle = senior.handle.to_string();
                    if handles.contains(&handle) {
                        continue;
                    }
                    handles.push(handle);
                    ids.extend(senior.account_ids(&platform).next().map(str::to_owned));
                }
                None => unknown.push(raw.to_owned()),
            }
        }
        if !unknown.is_empty() {
            let text = render::unknown_targets(l, &unknown, &self.roster);
            return self.say(chat, &text).await;
        }
        if handles.is_empty() {
            return self.say(chat, &render::invite_whom(l)).await;
        }
        if let Some(actions) = &mut self.preparing {
            actions.push(super::PendingAction::Invite {
                committee,
                group: agents,
                handles,
                ids,
            });
            return Ok(Flow::Done);
        }
        let operation = self.op_id(&chat.committee, chat.seq, &format!("{agents}#invite"))?;
        let text = match self
            .call(self.surface.invite(&agents, &ids, &operation))
            .await?
        {
            Outcome::Done(()) => render::invited(l, &handles),
            Outcome::Unsupported => render::invite_unsupported(l, &handles),
            Outcome::Refused => render::invite_failed(l, &handles),
        };
        self.say(chat, &text).await
    }

    /// 「agent 群只回答案」 / "answers only in the agent group": a Speaker
    /// `note` carrying the level, which the task board folds (the latest
    /// wins).
    pub(super) async fn set_output_level(
        &mut self,
        chat: &Event,
        level: OutputLevel,
    ) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        let committee = self.acting_committee(chat);
        if let Some(actions) = &mut self.preparing {
            actions.push(super::PendingAction::OutputLevel { committee, level });
            return Ok(Flow::Done);
        }
        let note = NewEvent::new(
            chat.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::output_level_note(l, level),
        )
        .with_id(derived_event_id(&[
            "output-level",
            chat.committee.as_str(),
            &chat.seq.to_string(),
        ]))
        .on_behalf_of(chat.from.clone())
        .with_meta(meta::OUTPUT_LEVEL, json!(level));
        self.append(note).await?;
        self.say(chat, &render::output_level_set(l, level)).await
    }

    /// An agent's bridge is active on the committee: invite its person into
    /// the Agent group, once per run and person (invites are idempotent;
    /// failures and surfaces without invites are ignored).
    pub(super) async fn invite_on_activity(&mut self, event: &Event) {
        let Some(handle) = event.from.as_agent().map(|a| a.owner().clone()) else {
            return;
        };
        let Some(state) = self.committees.get_mut(&event.committee) else {
            return;
        };
        let Some(agents) = state.agents_group().cloned() else {
            return;
        };
        if !state.invited.insert(handle.clone()) {
            return;
        }
        let Some(id) = self
            .roster
            .senior(&handle)
            .and_then(|s| s.account_ids(self.surface.platform()).next())
            .map(str::to_owned)
        else {
            return;
        };
        let Ok(operation) = self.op_id(
            &event.committee,
            agents.seq,
            &format!("{}#invite-{handle}", agents.group.group_id),
        ) else {
            return;
        };
        match self
            .call(
                self.surface
                    .invite(&agents.group.group_id, &[id], &operation),
            )
            .await
        {
            Ok(Outcome::Done(())) => debug!(%handle, "invited into the Agent group"),
            Ok(_) => {}
            Err(error) => {
                warn!(%handle, %error, "could not invite into the Agent group; will retry");
                if let Some(state) = self.committees.get_mut(&event.committee) {
                    state.invited.remove(&handle);
                }
            }
        }
    }
}

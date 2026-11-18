//! Shared work and pending reminders live alongside the public committee logs.

use std::collections::BTreeSet;

use senate_core::api::Secretaries;
use senate_core::secretary_work::{
    SecretaryReminder, SecretaryWorkEntry, SecretaryWorkItem, SecretaryWorkOp, SecretaryWorkReply,
    SecretaryWorkRequest, SecretaryWorkUpdate, SetSecretarySession, WorkThread,
};
use senate_core::{EventId, Hub, HubError, Name, Roster, SenatorName, StatusPayload, StatusState};
use serde::{Deserialize, Serialize};

use super::{ServerHub, log_error};

const PAGE_SIZE: usize = 16;
const MAX_NOTE_BYTES: usize = 4096;

pub(crate) enum SecretaryWorkError {
    Forbidden(String),
    Conflict(String),
    Hub(HubError),
}

impl From<HubError> for SecretaryWorkError {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    item: SecretaryWorkItem,
    entries: Vec<SecretaryWorkEntry>,
    pending: Option<SecretaryReminder>,
}

fn prefix(principal: &SenatorName) -> String {
    format!("secretary-work:{principal}:")
}

fn record_key(principal: &SenatorName, id: &EventId) -> String {
    format!("{}{id}", prefix(principal))
}

fn read_record(value: serde_json::Value) -> Result<Record, HubError> {
    serde_json::from_value(value).map_err(|e| HubError::Other(Box::new(e)))
}

fn team_exists(roster: &Roster, principal: &SenatorName) -> bool {
    roster.is_person(principal)
        || roster
            .office()
            .is_some_and(|office| office.speaker_here() == Some(principal))
}

fn owns(roster: &Roster, person: &Name, principal: &SenatorName) -> bool {
    principal == person || roster.owner_of(principal) == Some(person)
}

fn member(roster: &Roster, principal: &SenatorName, senator: &SenatorName) -> bool {
    senator.is_session_name()
        && roster.owner_of(senator).is_some()
        && roster.secretaries_of(principal).contains(senator)
}

fn visible_item(record: &Record, roster: &Roster, principal: &SenatorName) -> SecretaryWorkItem {
    let mut item = record.item.clone();
    item.owner = item.owner.filter(|owner| member(roster, principal, owner));
    item
}

fn missing(id: &EventId) -> HubError {
    HubError::NotFound(format!("no secretary work item {id}"))
}

fn can_manage(owner: Option<&SenatorName>, actor: &SenatorName, principal: bool) -> bool {
    principal || owner == Some(actor)
}

impl ServerHub {
    /// Restore access while keeping assignments from an earlier tenure in history.
    pub(super) async fn clear_retired_assignments(
        &self,
        principal: &SenatorName,
        added: &[SenatorName],
    ) -> Result<(), HubError> {
        if added.is_empty() {
            return Ok(());
        }
        for (_, value) in self
            .store
            .registry(&prefix(principal))
            .await
            .map_err(log_error)?
        {
            let mut record = read_record(value)?;
            if record
                .item
                .owner
                .as_ref()
                .is_some_and(|owner| added.contains(owner))
            {
                record.item.owner = None;
                record.pending = None;
                self.save_work(principal, &record).await?;
            }
        }
        Ok(())
    }

    fn may_appoint(
        &self,
        roster: &Roster,
        person: &Name,
        principal: &SenatorName,
        acted_by: Option<&SenatorName>,
    ) -> bool {
        owns(roster, person, principal)
            || acted_by.is_some_and(|senator| {
                self.built_in() != Some(senator) && roster.may_act_for_office(person, senator)
            })
    }

    pub(crate) async fn replace_secretaries(
        &self,
        person: &Name,
        principal: SenatorName,
        secretaries: Vec<SenatorName>,
        acted_by: Option<SenatorName>,
    ) -> Result<Secretaries, SecretaryWorkError> {
        let _guard = self.secretary_gate.lock().await;
        let roster = self.roster_now();
        if !self.may_appoint(&roster, person, &principal, acted_by.as_ref()) {
            return Err(SecretaryWorkError::Forbidden(format!(
                "{person} may not register secretaries for senator::{principal}: the senator itself, the principal that runs it, or the Speaker's office does"
            )));
        }
        for secretary in &secretaries {
            if !roster.knows(secretary) && self.built_in() != Some(secretary) {
                self.act_as(person, secretary)
                    .await
                    .map_err(|error| SecretaryWorkError::Forbidden(error.to_string()))?;
            }
        }
        Ok(self.store_secretaries(principal, secretaries).await?)
    }

    /// Change one entry of the existing ordered secretary list.
    pub(crate) async fn set_secretary_session(
        &self,
        person: &Name,
        principal: SenatorName,
        request: SetSecretarySession,
    ) -> Result<Secretaries, SecretaryWorkError> {
        let _guard = self.secretary_gate.lock().await;
        let roster = self.roster_now();
        if !team_exists(&roster, &principal)
            || !self.may_appoint(&roster, person, &principal, request.acted_by.as_ref())
        {
            return Err(SecretaryWorkError::Forbidden(format!(
                "{person} may not appoint sessions for {principal}"
            )));
        }
        let senator = request.senator;
        if !senator.is_session_name() {
            return Err(HubError::Rejected(
                "name a registered native session's primary senator".into(),
            )
            .into());
        }
        if request.appointed {
            let Some(owner) = roster
                .owner_of(&senator)
                .filter(|_| roster.built_in() != Some(&senator))
            else {
                return Err(HubError::Rejected(
                    "a member needs an established session owner".into(),
                )
                .into());
            };
            super::registry::put_seat(self.store.as_ref(), &senator, owner)
                .await
                .map_err(log_error)?;
        }
        let mut list = roster.secretaries_of(&principal).to_vec();
        if request.appointed && !list.contains(&senator) {
            list.push(senator);
        } else if !request.appointed {
            list.retain(|member| member != &senator);
        }
        Ok(self.store_secretaries(principal, list).await?)
    }

    pub(crate) async fn secretary_work(
        &self,
        person: &Name,
        principal: SenatorName,
        request: SecretaryWorkRequest,
    ) -> Result<SecretaryWorkReply, SecretaryWorkError> {
        // Membership, the current owner and the durable operation are read
        // together. A later writer observes the stored result of this one.
        let _guard = self.secretary_gate.lock().await;
        let roster = self.roster_now();
        let actor = request.actor;
        let as_owner = &actor == person && owns(&roster, person, &principal);
        let as_speaker = actor == principal
            && roster.owner_of(&actor) == Some(person)
            && roster
                .office()
                .is_some_and(|office| office.speaker_here() == Some(&actor))
            && self.built_in() != Some(&actor);
        let manager = as_owner || as_speaker;
        if !team_exists(&roster, &principal)
            || !(manager
                || (member(&roster, &principal, &actor) && roster.owner_of(&actor) == Some(person)))
        {
            return Err(SecretaryWorkError::Forbidden(format!(
                "{actor} has no membership in {principal}'s secretary team"
            )));
        }
        match request.operation {
            SecretaryWorkOp::Members => Ok(SecretaryWorkReply::Members {
                members: roster
                    .secretaries_of(&principal)
                    .iter()
                    .filter(|senator| member(&roster, &principal, senator))
                    .cloned()
                    .collect(),
                principal,
            }),
            SecretaryWorkOp::List { after } => {
                let list = self
                    .store
                    .registry(&prefix(&principal))
                    .await
                    .map_err(log_error)?;
                let after = after.map(|id| record_key(&principal, &id));
                let mut items = list
                    .into_iter()
                    .filter(|(key, _)| after.as_ref().is_none_or(|after| key > after))
                    .take(PAGE_SIZE + 1)
                    .map(|(_, value)| {
                        read_record(value).map(|record| visible_item(&record, &roster, &principal))
                    })
                    .collect::<Result<Vec<_>, HubError>>()?;
                let more = items.len() > PAGE_SIZE;
                items.truncate(PAGE_SIZE);
                let next = items.last().filter(|_| more).map(|item| item.id.clone());
                Ok(SecretaryWorkReply::List { items, next })
            }
            SecretaryWorkOp::Read { item, offset } => {
                let record = self
                    .load_work(&principal, &item)
                    .await?
                    .ok_or_else(|| missing(&item))?;
                let view = visible_item(&record, &roster, &principal);
                let mut entries: Vec<_> = record
                    .entries
                    .into_iter()
                    .skip(offset)
                    .take(PAGE_SIZE + 1)
                    .collect();
                let next = (entries.len() > PAGE_SIZE).then(|| offset + PAGE_SIZE);
                entries.truncate(PAGE_SIZE);
                Ok(SecretaryWorkReply::History {
                    item: view,
                    entries,
                    next,
                })
            }
            SecretaryWorkOp::Create { id, title, thread } => {
                self.create_work(&roster, principal, actor, id, title, thread)
                    .await
            }
            SecretaryWorkOp::Change { item, id, update } => {
                let record = self
                    .load_work(&principal, &item)
                    .await?
                    .ok_or_else(|| missing(&item))?;
                self.change_work(&roster, principal, actor, manager, record, id, update)
                    .await
            }
        }
    }

    async fn load_work(
        &self,
        principal: &SenatorName,
        id: &EventId,
    ) -> Result<Option<Record>, HubError> {
        let key = record_key(principal, id);
        self.store
            .registry(&key)
            .await
            .map_err(log_error)?
            .into_iter()
            .find(|(name, _)| name == &key)
            .map(|(_, value)| read_record(value))
            .transpose()
    }

    async fn save_work(&self, principal: &SenatorName, record: &Record) -> Result<(), HubError> {
        let value = serde_json::to_value(record).map_err(|e| HubError::Other(Box::new(e)))?;
        self.store
            .put_registry(&record_key(principal, &record.item.id), Some(value))
            .await
            .map_err(log_error)
    }

    async fn create_work(
        &self,
        roster: &Roster,
        principal: SenatorName,
        actor: SenatorName,
        id: EventId,
        title: String,
        thread: Option<WorkThread>,
    ) -> Result<SecretaryWorkReply, SecretaryWorkError> {
        let title = title.trim().to_owned();
        if let Some(record) = self.load_work(&principal, &id).await? {
            if record.item.created_by != actor
                || record.item.title != title
                || record.item.thread != thread
            {
                return Err(SecretaryWorkError::Conflict(
                    "work id already belongs to another operation".into(),
                ));
            }
            return Ok(SecretaryWorkReply::Saved {
                item: visible_item(&record, roster, &principal),
                duplicate: true,
            });
        }
        if title.is_empty() {
            return Err(HubError::Rejected("work needs a title".into()).into());
        }
        let mut status = StatusPayload::new(StatusState::Processing);
        status.title = Some(title.clone());
        status.check().map_err(HubError::Rejected)?;
        if let Some(reference) = &thread
            && !self
                .get(&reference.committee, reference.root)
                .await?
                .is_some_and(|event| event.thread.is_none())
        {
            return Err(HubError::Rejected(
                "source must name an existing root on its committee".into(),
            )
            .into());
        }
        let now = senate_log::now();
        let record = Record {
            item: SecretaryWorkItem {
                id,
                title,
                thread,
                created_by: actor,
                created_at: now,
                state: status.state,
                owner: None,
                steps: Vec::new(),
                updated_at: now,
            },
            entries: Vec::new(),
            pending: None,
        };
        self.save_work(&principal, &record).await?;
        Ok(SecretaryWorkReply::Saved {
            item: record.item,
            duplicate: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn change_work(
        &self,
        roster: &Roster,
        principal: SenatorName,
        actor: SenatorName,
        manager: bool,
        mut record: Record,
        id: EventId,
        update: SecretaryWorkUpdate,
    ) -> Result<SecretaryWorkReply, SecretaryWorkError> {
        if let Some(entry) = record.entries.iter().find(|entry| entry.id == id) {
            if entry.by != actor || entry.update != update {
                return Err(SecretaryWorkError::Conflict(
                    "operation id already used".into(),
                ));
            }
            return Ok(SecretaryWorkReply::Saved {
                item: visible_item(&record, roster, &principal),
                duplicate: true,
            });
        }
        if record.item.id == id {
            return Err(SecretaryWorkError::Conflict(
                "operation id already used to create this item".into(),
            ));
        }
        let owner = record
            .item
            .owner
            .as_ref()
            .filter(|owner| member(roster, &principal, owner));
        let manages = can_manage(owner, &actor, manager);
        let conflict = || {
            SecretaryWorkError::Conflict(match owner {
                Some(owner) => format!("item is owned by {owner}"),
                None => "claim the item before updating or handing it off".into(),
            })
        };
        let assignment = match &update {
            SecretaryWorkUpdate::Claim if member(roster, &principal, &actor) => {
                if owner.is_some_and(|owner| owner != &actor) {
                    return Err(conflict());
                }
                if owner.is_none() {
                    record.item.owner = Some(actor.clone());
                    record.pending = None;
                }
                None
            }
            SecretaryWorkUpdate::Claim => {
                return Err(SecretaryWorkError::Forbidden(
                    "only member sessions can claim items".into(),
                ));
            }
            SecretaryWorkUpdate::Assign { to } => {
                if owner.is_some() && !manages {
                    return Err(conflict());
                }
                Some(Some(to))
            }
            SecretaryWorkUpdate::Progress { state, steps } => {
                if !manages {
                    return Err(conflict());
                }
                let mut status = StatusPayload::new(*state);
                status.steps.clone_from(steps);
                status.check().map_err(HubError::Rejected)?;
                // Each page of history must fit through the bridge IPC frame.
                if let Some(steps) = steps
                    && steps
                        .iter()
                        .map(|step| step.id.len() + step.label.len())
                        .sum::<usize>()
                        > MAX_NOTE_BYTES
                {
                    return Err(HubError::Rejected(format!(
                        "progress steps need at most {MAX_NOTE_BYTES} UTF-8 bytes"
                    ))
                    .into());
                }
                record.item.state = *state;
                if let Some(steps) = steps {
                    record.item.steps.clone_from(steps);
                }
                None
            }
            SecretaryWorkUpdate::Note { text } => {
                check_note(text)?;
                None
            }
            SecretaryWorkUpdate::Handoff { to, text } => {
                if !manages {
                    return Err(conflict());
                }
                check_note(text)?;
                Some(to.as_ref())
            }
        };
        if let Some(next) = assignment {
            if next.is_some_and(|to| !member(roster, &principal, to)) {
                return Err(HubError::Rejected(
                    "the destination must be a current secretary session on this team".into(),
                )
                .into());
            }
            record.item.owner = next.cloned();
            record.pending = next.filter(|to| *to != &actor).map(|to| SecretaryReminder {
                principal: principal.clone(),
                item: record.item.id.clone(),
                id: id.clone(),
                to: to.clone(),
            });
        }
        let now = senate_log::now();
        record.item.updated_at = now;
        record.entries.push(SecretaryWorkEntry {
            id,
            by: actor,
            at: now,
            update,
        });
        self.save_work(&principal, &record).await?;
        Ok(SecretaryWorkReply::Saved {
            item: visible_item(&record, roster, &principal),
            duplicate: false,
        })
    }

    pub(crate) async fn secretary_reminders(
        &self,
        person: &Name,
        senators: Vec<SenatorName>,
    ) -> Result<Vec<SecretaryReminder>, HubError> {
        let _guard = self.secretary_gate.lock().await;
        let roster = self.roster_now();
        let ours: BTreeSet<_> = senators
            .into_iter()
            .filter(|s| roster.owner_of(s) == Some(person))
            .collect();
        let mut reminders = Vec::new();
        for (principal, members) in roster.secretaries() {
            if !team_exists(&roster, principal) || !members.iter().any(|s| ours.contains(s)) {
                continue;
            }
            let rows = self
                .store
                .registry(&prefix(principal))
                .await
                .map_err(log_error)?;
            for (_, value) in rows {
                if let Some(value) = value.get("pending")
                    && !value.is_null()
                {
                    let reminder: SecretaryReminder = serde_json::from_value(value.clone())
                        .map_err(|e| HubError::Other(Box::new(e)))?;
                    if ours.contains(&reminder.to) && member(&roster, principal, &reminder.to) {
                        reminders.push(reminder);
                        if reminders.len() == PAGE_SIZE {
                            return Ok(reminders);
                        }
                    }
                }
            }
        }
        Ok(reminders)
    }

    pub(crate) async fn ack_secretary_reminder(
        &self,
        person: &Name,
        reminder: SecretaryReminder,
    ) -> Result<(), SecretaryWorkError> {
        let _guard = self.secretary_gate.lock().await;
        let roster = self.roster_now();
        if !team_exists(&roster, &reminder.principal)
            || !member(&roster, &reminder.principal, &reminder.to)
            || roster.owner_of(&reminder.to) != Some(person)
        {
            return Err(SecretaryWorkError::Forbidden(
                "the destination session no longer belongs to this team".into(),
            ));
        }
        let Some(mut record) = self.load_work(&reminder.principal, &reminder.item).await? else {
            return Ok(());
        };
        if record.pending.as_ref() == Some(&reminder) {
            record.pending = None;
            self.save_work(&reminder.principal, &record).await?;
        }
        Ok(())
    }
}

fn check_note(text: &str) -> Result<(), HubError> {
    if text.trim().is_empty() || text.len() > MAX_NOTE_BYTES {
        Err(HubError::Rejected(format!(
            "a note needs text within {MAX_NOTE_BYTES} UTF-8 bytes"
        )))
    } else {
        Ok(())
    }
}

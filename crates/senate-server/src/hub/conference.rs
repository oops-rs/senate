//! Server-side persistence, ACLs, write-back, and anti-loop floor enforcement
//! for cross-committee multi-party conferences.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use senate_core::conference::{
    AdjournConferenceRequest, ConferenceDetail, ConferenceId, ConferenceInvitePayload,
    ConferenceList, ConferenceReport, ConferenceReportPayload, ConferenceRsvpAction,
    ConferenceState, ConferenceSummary, CreateConferenceRequest, FloorState, InvitedSeat,
    LinkedThread, MAX_TOPIC_CHARS, PostConferenceEventRequest, PostConferenceEventResponse,
    SeatStatus,
};
use senate_core::mention::Mention;
use senate_core::{
    Address, CommitteeId, Hub, HubError, Kind, Locator, Name, NewEvent, Roster, SenatorName, meta,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ServerHub, log_error};
use crate::rules::{SPEAKER_KEYS, check_may_change_state};

const CONFERENCE_KEY_PREFIX: &str = "conference:";
const CONFERENCE_READ_LIMIT: usize = 500;

pub(crate) enum ConferenceError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    Hub(HubError),
}

impl From<HubError> for ConferenceError {
    fn from(error: HubError) -> Self {
        match error {
            HubError::Rejected(msg) => Self::BadRequest(msg),
            HubError::NotFound(msg) => Self::NotFound(msg),
            other => Self::Hub(other),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConferenceRecord {
    id: ConferenceId,
    topic: String,
    convener: Address,
    state: ConferenceState,
    seats: Vec<InvitedSeat>,
    #[serde(default)]
    linked_threads: Vec<LinkedThread>,
    floor: FloorState,
    created_at: DateTime<Utc>,
    #[serde(default)]
    adjourned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    report: Option<ConferenceReport>,
    #[serde(default)]
    invited_indices: BTreeSet<usize>,
}

impl ConferenceRecord {
    fn summary(&self) -> ConferenceSummary {
        ConferenceSummary {
            id: self.id.clone(),
            topic: self.topic.clone(),
            convener: self.convener.clone(),
            state: self.state,
            seats: self.seats.clone(),
            linked_threads: self.linked_threads.clone(),
            floor: self.floor.clone(),
            created_at: self.created_at,
            adjourned_at: self.adjourned_at,
            report: self.report.clone(),
        }
    }
}

fn registry_key(id: &ConferenceId) -> String {
    format!("{CONFERENCE_KEY_PREFIX}{id}")
}

fn deserialize_record(value: serde_json::Value) -> Result<ConferenceRecord, ConferenceError> {
    serde_json::from_value(value).map_err(|e| HubError::Other(Box::new(e)).into())
}

fn actor_senator(addr: &Address) -> Option<&SenatorName> {
    match addr {
        Address::Senator(s) => Some(s),
        _ => None,
    }
}

fn person_controls_senator(roster: &Roster, person: &Name, senator: &SenatorName) -> bool {
    senator.as_str() == person.as_str() || roster.owner_of(senator) == Some(person)
}

fn person_controls_address(roster: &Roster, person: &Name, addr: &Address) -> bool {
    actor_senator(addr).is_some_and(|s| person_controls_senator(roster, person, s))
}

fn extract_mentions(
    body: &str,
    explicit_to: &[Address],
    seated_agents: &[Address],
) -> Vec<Address> {
    let mut out = Vec::new();
    for addr in explicit_to {
        if !out.contains(addr) {
            out.push(addr.clone());
        }
    }
    for found in Mention::scan(body) {
        if let Some(addr) = found.mention.address() {
            if !out.contains(&addr) {
                out.push(addr);
            }
        } else if let Mention::Locator(locator) = found.mention {
            let addr = locator.address().clone();
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    for token in body.split_whitespace() {
        let Some(raw) = token.strip_prefix('@') else {
            continue;
        };
        let cleaned =
            raw.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        if cleaned.is_empty() {
            continue;
        }
        if let Some(matched) = seated_agents
            .iter()
            .find(|a| actor_senator(a).is_some_and(|s| s.as_str() == cleaned))
            && !out.contains(matched)
        {
            out.push(matched.clone());
        }
    }
    out
}

impl ServerHub {
    async fn load_conference(
        &self,
        id: &ConferenceId,
    ) -> Result<ConferenceRecord, ConferenceError> {
        let Some(val) = self
            .store
            .registry_value(&registry_key(id))
            .await
            .map_err(log_error)?
        else {
            return Err(ConferenceError::NotFound(format!(
                "no such conference {id}"
            )));
        };
        deserialize_record(val)
    }

    async fn save_conference(&self, record: &ConferenceRecord) -> Result<(), ConferenceError> {
        let val = serde_json::to_value(record).map_err(|e| HubError::Other(Box::new(e)))?;
        self.store
            .put_registry(&registry_key(&record.id), Some(val))
            .await
            .map_err(log_error)?;
        Ok(())
    }

    async fn agent_connected_anywhere(&self, agent: &SenatorName) -> Result<bool, HubError> {
        for committee in self.committees().await? {
            if self.presence.is_connected(&committee, agent) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn resolve_seat_initial(
        &self,
        locator: &Locator,
        convener: &Address,
        roster: &Roster,
    ) -> Result<(InvitedSeat, Option<Address>), ConferenceError> {
        match locator.address() {
            Address::Chair => {
                let Some(committee) = locator.committee().cloned() else {
                    return Err(ConferenceError::BadRequest(format!(
                        "chair seat must name a committee: {locator}"
                    )));
                };
                if !self
                    .store
                    .committees()
                    .await
                    .map_err(log_error)?
                    .contains(&committee)
                {
                    return Ok((
                        InvitedSeat {
                            target: locator.clone(),
                            committee: Some(committee),
                            status: SeatStatus::Absent,
                        },
                        None,
                    ));
                }
                let info = self.chair(&committee).await?;
                let Some(chair_senator) = info.chair else {
                    return Ok((
                        InvitedSeat {
                            target: locator.clone(),
                            committee: Some(committee),
                            status: SeatStatus::Absent,
                        },
                        None,
                    ));
                };
                let chair_addr = Address::Senator(chair_senator.clone());
                let online = roster.is_person(&chair_senator)
                    || self.presence.is_connected(&committee, &chair_senator);
                let status = if online {
                    SeatStatus::Pending
                } else {
                    SeatStatus::Absent
                };
                Ok((
                    InvitedSeat {
                        target: locator.clone(),
                        committee: Some(committee),
                        status,
                    },
                    online.then_some(chair_addr),
                ))
            }
            Address::Senator(senator) => {
                if !roster.knows(senator) {
                    return Err(ConferenceError::BadRequest(format!(
                        "{locator} is not a known senator in this Senate"
                    )));
                }
                let committee = locator.committee().cloned();
                let addr = Address::Senator(senator.clone());
                if &addr == convener {
                    return Ok((
                        InvitedSeat {
                            target: locator.clone(),
                            committee,
                            status: SeatStatus::Seated {
                                participant: addr,
                                delegated_by: None,
                                ephemeral: false,
                            },
                        },
                        None,
                    ));
                }
                let online = if roster.is_person(senator) {
                    true
                } else if let Some(ref c) = committee {
                    self.presence.is_connected(c, senator)
                } else {
                    self.agent_connected_anywhere(senator).await?
                };
                let status = if online {
                    SeatStatus::Pending
                } else {
                    SeatStatus::Absent
                };
                Ok((
                    InvitedSeat {
                        target: locator.clone(),
                        committee,
                        status,
                    },
                    online.then_some(addr),
                ))
            }
            _ => Err(ConferenceError::BadRequest(format!(
                "conference seats must name <committee>::chair or senator::<name>: {locator}"
            ))),
        }
    }

    async fn resolve_seat_holder(
        &self,
        seat: &InvitedSeat,
        roster: &Roster,
    ) -> Result<Option<Address>, ConferenceError> {
        match seat.target.address() {
            Address::Chair => {
                let Some(ref committee) = seat.committee else {
                    return Ok(None);
                };
                if !self
                    .store
                    .committees()
                    .await
                    .map_err(log_error)?
                    .contains(committee)
                {
                    return Ok(None);
                }
                Ok(self.chair(committee).await?.chair.map(Address::Senator))
            }
            Address::Senator(senator) => {
                let _ = roster;
                Ok(Some(Address::Senator(senator.clone())))
            }
            _ => Ok(None),
        }
    }

    async fn can_person_access_seat(
        &self,
        seat: &InvitedSeat,
        person: &Name,
        roster: &Roster,
    ) -> Result<bool, ConferenceError> {
        if let SeatStatus::Seated {
            participant,
            delegated_by,
            ..
        } = &seat.status
        {
            if person_controls_address(roster, person, participant) {
                return Ok(true);
            }
            if let Some(delegator) = delegated_by
                && person_controls_address(roster, person, delegator)
            {
                return Ok(true);
            }
        }
        if let Some(holder) = self.resolve_seat_holder(seat, roster).await?
            && person_controls_address(roster, person, &holder)
        {
            return Ok(true);
        }
        Ok(false)
    }

    async fn can_person_access_conference(
        &self,
        record: &ConferenceRecord,
        person: &Name,
        roster: &Roster,
    ) -> Result<bool, ConferenceError> {
        if person_controls_address(roster, person, &record.convener) {
            return Ok(true);
        }
        for seat in &record.seats {
            if self.can_person_access_seat(seat, person, roster).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn send_committee_invite(
        &self,
        committee: &CommitteeId,
        recipient: &Address,
        record: &ConferenceRecord,
        target: &Locator,
    ) -> Result<(), ConferenceError> {
        let payload = ConferenceInvitePayload {
            conference_id: record.id.clone(),
            topic: record.topic.clone(),
            convener: record.convener.clone(),
            target: Some(target.clone()),
            linked_threads: record.linked_threads.clone(),
        };
        let invite_json =
            serde_json::to_value(&payload).map_err(|e| HubError::Other(Box::new(e)))?;
        let body = format!(
            "Conference invited: {} ({}) — senate://conference/{}",
            record.topic, target, record.id
        );
        let event = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, body)
            .with_to(vec![recipient.clone()])
            .with_meta(meta::CONFERENCE_ID, json!(record.id))
            .with_meta(meta::CONFERENCE_INVITE, invite_json);
        self.commit(event).await?;
        Ok(())
    }

    async fn refresh_absent_seats(
        &self,
        record: &mut ConferenceRecord,
    ) -> Result<bool, ConferenceError> {
        if record.state != ConferenceState::Convening {
            return Ok(false);
        }
        let roster = self.roster_now();
        let mut changed = false;
        let mut pending_invites: Vec<(usize, CommitteeId, Address, Locator)> = Vec::new();
        for idx in 0..record.seats.len() {
            if !matches!(record.seats[idx].status, SeatStatus::Absent) {
                continue;
            }
            let target = record.seats[idx].target.clone();
            let seat_committee = record.seats[idx].committee.clone();
            let (refreshed, notify_target) = match target.address() {
                Address::Chair => {
                    let Some(ref committee) = seat_committee else {
                        continue;
                    };
                    if !self
                        .store
                        .committees()
                        .await
                        .map_err(log_error)?
                        .contains(committee)
                    {
                        continue;
                    }
                    let Some(chair_senator) = self.chair(committee).await?.chair else {
                        continue;
                    };
                    let online = roster.is_person(&chair_senator)
                        || self.presence.is_connected(committee, &chair_senator);
                    (online, online.then_some(Address::Senator(chair_senator)))
                }
                Address::Senator(senator) => {
                    if !roster.knows(senator) {
                        continue;
                    }
                    let online = if roster.is_person(senator) {
                        true
                    } else if let Some(ref c) = seat_committee {
                        self.presence.is_connected(c, senator)
                    } else {
                        self.agent_connected_anywhere(senator).await?
                    };
                    (online, online.then_some(Address::Senator(senator.clone())))
                }
                _ => (false, None),
            };
            if refreshed {
                record.seats[idx].status = SeatStatus::Pending;
                changed = true;
                if !record.invited_indices.contains(&idx)
                    && let (Some(committee), Some(recipient)) = (seat_committee, notify_target)
                {
                    pending_invites.push((idx, committee, recipient, target));
                }
            }
        }
        for (idx, committee, recipient, target) in pending_invites {
            self.send_committee_invite(&committee, &recipient, record, &target)
                .await?;
            record.invited_indices.insert(idx);
        }
        if changed {
            self.save_conference(record).await?;
        }
        Ok(changed)
    }

    async fn commit_conference_state_note(
        &self,
        record: &ConferenceRecord,
        body: impl Into<String>,
        to: Vec<Address>,
        extra_meta: Option<(&str, serde_json::Value)>,
    ) -> Result<(), ConferenceError> {
        let state_val =
            serde_json::to_value(record.state).map_err(|e| HubError::Other(Box::new(e)))?;
        let mut event = NewEvent::new(
            record.id.as_committee_id(),
            Kind::Note,
            Address::Speaker,
            body,
        )
        .with_to(to)
        .with_meta(meta::CONFERENCE_ID, json!(record.id))
        .with_meta(meta::CONFERENCE_STATE, state_val);
        if let Some((k, v)) = extra_meta {
            event = event.with_meta(k, v);
        }
        self.commit(event).await?;
        Ok(())
    }

    pub(crate) async fn convene_conference(
        &self,
        person: &Name,
        request: CreateConferenceRequest,
    ) -> Result<ConferenceSummary, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let topic = request.topic.trim().to_owned();
        if topic.is_empty() {
            return Err(ConferenceError::BadRequest(
                "conference topic cannot be empty".into(),
            ));
        }
        if topic.chars().count() > MAX_TOPIC_CHARS {
            return Err(ConferenceError::BadRequest(format!(
                "conference topic exceeds {MAX_TOPIC_CHARS} characters"
            )));
        }
        if request.seats.is_empty() {
            return Err(ConferenceError::BadRequest(
                "conference must invite at least one seat".into(),
            ));
        }

        let convener = match request.from {
            Some(Address::Senator(ref senator)) => {
                self.act_as(person, senator)
                    .await
                    .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
                Address::Senator(senator.clone())
            }
            Some(other) => {
                return Err(ConferenceError::BadRequest(format!(
                    "conference convener must be senator::<name>, not {other}"
                )));
            }
            None => Address::Senator(
                SenatorName::new(person.clone())
                    .map_err(|e| ConferenceError::BadRequest(e.to_string()))?,
            ),
        };

        let roster = self.roster_now();
        let id = ConferenceId::fresh();
        let mut seats = Vec::with_capacity(request.seats.len());
        let mut pending_invites: Vec<(usize, CommitteeId, Address, Locator)> = Vec::new();
        let mut seen_targets = BTreeSet::new();

        for input in request.seats {
            let key = input.target.to_string();
            if !seen_targets.insert(key) {
                continue;
            }
            let (seat, notify_recipient) = self
                .resolve_seat_initial(&input.target, &convener, &roster)
                .await?;
            let idx = seats.len();
            if matches!(seat.status, SeatStatus::Pending)
                && let (Some(committee), Some(recipient)) =
                    (seat.committee.clone(), notify_recipient)
            {
                pending_invites.push((idx, committee, recipient, seat.target.clone()));
            }
            seats.push(seat);
        }

        let all_seated = seats
            .iter()
            .all(|s| matches!(s.status, SeatStatus::Seated { .. }));
        let initial_state = if all_seated {
            ConferenceState::InSession
        } else {
            ConferenceState::Convening
        };

        let mut record = ConferenceRecord {
            id: id.clone(),
            topic: topic.clone(),
            convener: convener.clone(),
            state: initial_state,
            seats,
            linked_threads: request.linked_threads,
            floor: FloorState::default(),
            created_at: Utc::now(),
            adjourned_at: None,
            report: None,
            invited_indices: pending_invites.iter().map(|(i, ..)| *i).collect(),
        };

        self.save_conference(&record).await?;
        let state_msg = if initial_state == ConferenceState::InSession {
            format!("Conference in session: {topic} — senate://conference/{id}")
        } else {
            format!("Conference convened: {topic}")
        };
        self.commit_conference_state_note(&record, state_msg, vec![], None)
            .await?;

        for (_, committee, recipient, target) in pending_invites {
            self.send_committee_invite(&committee, &recipient, &record, &target)
                .await?;
        }
        // Keep record fresh if committees just materialized.
        let _ = self.refresh_absent_seats(&mut record).await?;
        Ok(record.summary())
    }

    pub(crate) async fn list_conferences(
        &self,
        person: &Name,
    ) -> Result<ConferenceList, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let roster = self.roster_now();
        let entries = self
            .store
            .registry(CONFERENCE_KEY_PREFIX)
            .await
            .map_err(log_error)?;
        let mut conferences = Vec::new();
        for (_, value) in entries {
            let mut record = deserialize_record(value)?;
            let _ = self.refresh_absent_seats(&mut record).await?;
            if self
                .can_person_access_conference(&record, person, &roster)
                .await?
            {
                conferences.push(record.summary());
            }
        }
        conferences.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(ConferenceList { conferences })
    }

    pub(crate) async fn get_conference(
        &self,
        person: &Name,
        id: &ConferenceId,
    ) -> Result<ConferenceDetail, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let roster = self.roster_now();
        let mut record = self.load_conference(id).await?;
        let _ = self.refresh_absent_seats(&mut record).await?;
        if !self
            .can_person_access_conference(&record, person, &roster)
            .await?
        {
            return Err(ConferenceError::Forbidden(format!(
                "{person} is not a participant in conference {id}"
            )));
        }
        let events = self
            .read(
                &record.id.as_committee_id(),
                Default::default(),
                CONFERENCE_READ_LIMIT,
            )
            .await?;
        Ok(ConferenceDetail {
            summary: record.summary(),
            events,
        })
    }

    pub(crate) async fn authorize_conference_stream(
        &self,
        person: &Name,
        id: &ConferenceId,
        senator: Option<&SenatorName>,
    ) -> Result<Option<SenatorName>, ConferenceError> {
        let roster = self.roster_now();
        let record = self.load_conference(id).await?;
        if !self
            .can_person_access_conference(&record, person, &roster)
            .await?
        {
            return Err(ConferenceError::Forbidden(format!(
                "{person} is not a participant in conference {id}"
            )));
        }
        let Some(senator) = senator else {
            return Ok(None);
        };
        self.act_as(person, senator)
            .await
            .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
        let addr = Address::Senator(senator.clone());
        let is_participant = record.convener == addr
            || record.seats.iter().any(|s| match &s.status {
                SeatStatus::Seated { participant, .. } => participant == &addr,
                _ => false,
            });
        if !is_participant {
            return Err(ConferenceError::Forbidden(format!(
                "{addr} does not hold an active seat in conference {id}"
            )));
        }
        Ok((!roster.is_person(senator)).then(|| senator.clone()))
    }

    pub(crate) async fn rsvp_conference(
        &self,
        person: &Name,
        id: &ConferenceId,
        action: ConferenceRsvpAction,
    ) -> Result<ConferenceSummary, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let mut record = self.load_conference(id).await?;
        if record.state == ConferenceState::Adjourned {
            return Err(ConferenceError::Conflict(format!(
                "conference {id} is already adjourned"
            )));
        }
        let _ = self.refresh_absent_seats(&mut record).await?;
        let roster = self.roster_now();

        let requested_target = match &action {
            ConferenceRsvpAction::Attend { target, .. } => target.as_ref(),
            ConferenceRsvpAction::Delegate { target, .. } => target.as_ref(),
        };

        let mut matched_idx = None;
        for (idx, seat) in record.seats.iter().enumerate() {
            if let Some(t) = requested_target
                && &seat.target != t
            {
                continue;
            }
            if self.can_person_access_seat(seat, person, &roster).await? {
                matched_idx = Some(idx);
                break;
            }
        }

        let Some(idx) = matched_idx else {
            return Err(ConferenceError::Forbidden(format!(
                "{person} does not hold an invited seat matching this RSVP on conference {id}"
            )));
        };

        let seat_target = record.seats[idx].target.clone();
        let seat_committee = record.seats[idx].committee.clone();
        let holder = self
            .resolve_seat_holder(&record.seats[idx], &roster)
            .await?;

        let note_body = match action {
            ConferenceRsvpAction::Attend { participant, .. } => {
                let participant_addr = match participant {
                    Some(Address::Senator(s)) => {
                        self.act_as(person, &s)
                            .await
                            .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
                        Address::Senator(s)
                    }
                    Some(other) => {
                        return Err(ConferenceError::BadRequest(format!(
                            "participant must be senator::<name>, not {other}"
                        )));
                    }
                    None => holder.unwrap_or_else(|| {
                        Address::Senator(
                            SenatorName::new(person.clone()).expect("person Name is SenatorName"),
                        )
                    }),
                };
                if let Some(s) = actor_senator(&participant_addr) {
                    self.act_as(person, s)
                        .await
                        .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
                }
                record.seats[idx].status = SeatStatus::Seated {
                    participant: participant_addr.clone(),
                    delegated_by: None,
                    ephemeral: false,
                };
                format!("{participant_addr} attended seat {seat_target}")
            }
            ConferenceRsvpAction::Delegate {
                senator,
                delegated_by,
                brief,
                ephemeral,
                ..
            } => {
                let Address::Senator(ref del_name) = senator else {
                    return Err(ConferenceError::BadRequest(format!(
                        "delegate senator must be senator::<name>, not {senator}"
                    )));
                };
                self.act_as(person, del_name)
                    .await
                    .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
                let delegator = match delegated_by {
                    Some(addr) => {
                        if !person_controls_address(&roster, person, &addr) {
                            return Err(ConferenceError::Forbidden(format!(
                                "{person} cannot delegate on behalf of {addr}"
                            )));
                        }
                        addr
                    }
                    None => holder.unwrap_or_else(|| {
                        Address::Senator(
                            SenatorName::new(person.clone()).expect("person Name is SenatorName"),
                        )
                    }),
                };
                record.seats[idx].status = SeatStatus::Seated {
                    participant: senator.clone(),
                    delegated_by: Some(delegator.clone()),
                    ephemeral,
                };

                // Scoped delegation context: only this seat's own committee brief, never unrelated logs.
                let committee_context = match seat_committee.as_ref() {
                    Some(c) => match self.brief(c).await {
                        Ok(b) => Some(format!(
                            "committee `{}` brief: {} open questions, {} decisions",
                            c,
                            b.open_questions.len(),
                            b.decisions.len()
                        )),
                        Err(_) => Some(format!("committee `{}`", c)),
                    },
                    None => None,
                };
                let mut lines = vec![format!(
                    "{delegator} delegated seat {seat_target} to {senator}{} (topic: {})",
                    if ephemeral { " [ephemeral]" } else { "" },
                    record.topic
                )];
                if let Some(note) = brief.filter(|s| !s.trim().is_empty()) {
                    lines.push(format!("delegation brief: {}", note.trim()));
                }
                if let Some(ctx) = committee_context {
                    lines.push(format!("scoped context: {ctx}"));
                }
                lines.join(" | ")
            }
        };

        let all_seated = !record.seats.is_empty()
            && record
                .seats
                .iter()
                .all(|s| matches!(s.status, SeatStatus::Seated { .. }));
        let transitioned = if record.state == ConferenceState::Convening && all_seated {
            record.state = ConferenceState::InSession;
            true
        } else {
            false
        };

        self.save_conference(&record).await?;
        self.commit_conference_state_note(&record, note_body, vec![], None)
            .await?;

        if transitioned {
            let mut seated_targets: Vec<Address> = record
                .seats
                .iter()
                .filter_map(|s| match &s.status {
                    SeatStatus::Seated { participant, .. } => Some(participant.clone()),
                    _ => None,
                })
                .collect();
            if !seated_targets.contains(&record.convener) {
                seated_targets.push(record.convener.clone());
            }
            let session_msg = format!(
                "Conference in session (quorum reached): {} — senate://conference/{}",
                record.topic, record.id
            );
            self.commit_conference_state_note(&record, session_msg, seated_targets, None)
                .await?;
        }

        Ok(record.summary())
    }

    pub(crate) async fn start_conference(
        &self,
        person: &Name,
        id: &ConferenceId,
    ) -> Result<ConferenceSummary, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let mut record = self.load_conference(id).await?;
        let roster = self.roster_now();
        if !person_controls_address(&roster, person, &record.convener) {
            return Err(ConferenceError::Forbidden(format!(
                "only the convener ({}) may start conference {id} early",
                record.convener
            )));
        }
        if record.state == ConferenceState::Adjourned {
            return Err(ConferenceError::Conflict(format!(
                "conference {id} is already adjourned"
            )));
        }
        if record.state == ConferenceState::InSession {
            return Ok(record.summary());
        }
        record.state = ConferenceState::InSession;
        self.save_conference(&record).await?;

        let seated_targets: Vec<Address> = record
            .seats
            .iter()
            .filter_map(|s| match &s.status {
                SeatStatus::Seated { participant, .. } => Some(participant.clone()),
                _ => None,
            })
            .collect();
        self.commit_conference_state_note(
            &record,
            format!(
                "Conference started by convener (bypassing absent/pending seats): {} — senate://conference/{}",
                record.topic, record.id
            ),
            seated_targets,
            None,
        )
        .await?;
        Ok(record.summary())
    }

    pub(crate) async fn post_conference_event(
        &self,
        person: &Name,
        id: &ConferenceId,
        request: PostConferenceEventRequest,
    ) -> Result<PostConferenceEventResponse, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let mut record = self.load_conference(id).await?;
        match record.state {
            ConferenceState::Adjourned => {
                return Err(ConferenceError::Conflict(format!(
                    "conference {id} is adjourned and read-only"
                )));
            }
            ConferenceState::Convening => {
                return Err(ConferenceError::Conflict(format!(
                    "conference {id} is still convening; reach quorum or start first"
                )));
            }
            ConferenceState::InSession => {}
        }

        let Address::Senator(ref sender_name) = request.from else {
            return Err(ConferenceError::Forbidden(format!(
                "conference messages must be posted by senator::<name>, not {}",
                request.from
            )));
        };
        self.act_as(person, sender_name)
            .await
            .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;

        let is_participant = record.convener == request.from
            || record.seats.iter().any(|s| match &s.status {
                SeatStatus::Seated { participant, .. } => participant == &request.from,
                _ => false,
            });
        if !is_participant {
            return Err(ConferenceError::Forbidden(format!(
                "{} does not hold an active seat in conference {id}",
                request.from
            )));
        }

        if request.body.trim().is_empty() {
            return Err(ConferenceError::BadRequest(
                "conference message body cannot be empty".into(),
            ));
        }

        let roster = self.roster_now();
        let from_is_human = roster.is_person(sender_name);

        let mut seated_agents = Vec::new();
        if let Some(conv_name) = actor_senator(&record.convener)
            && !roster.is_person(conv_name)
        {
            seated_agents.push(record.convener.clone());
        }
        for seat in &record.seats {
            if let SeatStatus::Seated { participant, .. } = &seat.status
                && let Some(agent_name) = actor_senator(participant)
                && !roster.is_person(agent_name)
                && !seated_agents.contains(participant)
            {
                seated_agents.push(participant.clone());
            }
        }

        let mentioned = extract_mentions(&request.body, &request.to, &seated_agents);
        let push_targets =
            record
                .floor
                .advance(&request.from, from_is_human, &mentioned, &seated_agents);

        let kind = request.kind.unwrap_or(Kind::Note);
        let mut new_event = NewEvent::new(
            record.id.as_committee_id(),
            kind,
            request.from.clone(),
            request.body,
        );
        new_event.to = request.to;
        for (k, v) in request.meta {
            if SPEAKER_KEYS.contains(&k.as_str())
                || matches!(
                    k.as_str(),
                    meta::CONFERENCE_ID
                        | meta::CONFERENCE_INVITE
                        | meta::CONFERENCE_REPORT
                        | meta::CONFERENCE_STATE
                )
            {
                return Err(ConferenceError::Forbidden(format!(
                    "meta.{k} is reserved for the server"
                )));
            }
            new_event.meta.insert(k, v);
        }
        new_event
            .meta
            .insert(meta::CONFERENCE_ID.to_owned(), json!(record.id));
        check_may_change_state(&new_event).map_err(ConferenceError::BadRequest)?;

        let appended = self.commit(new_event).await?;
        self.save_conference(&record).await?;

        Ok(PostConferenceEventResponse {
            event: appended.event,
            duplicate: appended.duplicate,
            floor: record.floor,
            push_targets,
        })
    }

    pub(crate) async fn adjourn_conference(
        &self,
        person: &Name,
        id: &ConferenceId,
        request: AdjournConferenceRequest,
    ) -> Result<ConferenceSummary, ConferenceError> {
        let _guard = self.conference_gate.lock().await;
        let mut record = self.load_conference(id).await?;
        let roster = self.roster_now();
        if !self
            .can_person_access_conference(&record, person, &roster)
            .await?
        {
            return Err(ConferenceError::Forbidden(format!(
                "{person} is not a participant in conference {id}"
            )));
        }
        if record.state == ConferenceState::Adjourned {
            return Err(ConferenceError::Conflict(format!(
                "conference {id} is already adjourned"
            )));
        }

        let actor = match request.from {
            Some(Address::Senator(ref s)) => {
                self.act_as(person, s)
                    .await
                    .map_err(|e| ConferenceError::Forbidden(e.to_string()))?;
                Address::Senator(s.clone())
            }
            Some(other) => {
                return Err(ConferenceError::BadRequest(format!(
                    "adjourn sender must be senator::<name>, not {other}"
                )));
            }
            None => record.convener.clone(),
        };

        let report = ConferenceReport {
            summary: request.summary,
            decisions: request.decisions,
            action_items: request.action_items,
        };
        report.check().map_err(ConferenceError::BadRequest)?;

        let mut participants = vec![record.convener.clone()];
        let mut ephemeral_delegates = Vec::new();
        let mut committee_targets: Vec<(CommitteeId, Option<Address>)> = Vec::new();

        for seat in &record.seats {
            if let SeatStatus::Seated {
                participant,
                ephemeral,
                ..
            } = &seat.status
            {
                if !participants.contains(participant) {
                    participants.push(participant.clone());
                }
                if *ephemeral && !ephemeral_delegates.contains(participant) {
                    ephemeral_delegates.push(participant.clone());
                }
            }
            if let Some(ref committee) = seat.committee {
                let recipient = self.resolve_seat_holder(seat, &roster).await?;
                if !committee_targets.iter().any(|(c, _)| c == committee) {
                    committee_targets.push((committee.clone(), recipient));
                }
            }
        }

        let payload = ConferenceReportPayload {
            conference_id: record.id.clone(),
            topic: record.topic.clone(),
            convener: record.convener.clone(),
            participants,
            report: report.clone(),
            ephemeral_delegates: ephemeral_delegates.clone(),
        };
        let payload_json =
            serde_json::to_value(&payload).map_err(|e| HubError::Other(Box::new(e)))?;

        record.state = ConferenceState::Adjourned;
        record.adjourned_at = Some(Utc::now());
        record.report = Some(report.clone());
        self.save_conference(&record).await?;

        let report_body = format!(
            "Conference adjourned ({}) by {}: {}",
            record.topic, actor, report.summary
        );
        self.commit_conference_state_note(
            &record,
            report_body.clone(),
            ephemeral_delegates.clone(),
            Some((meta::CONFERENCE_REPORT, payload_json.clone())),
        )
        .await?;

        for (committee, recipient) in committee_targets {
            if !self
                .store
                .committees()
                .await
                .map_err(log_error)?
                .contains(&committee)
            {
                continue;
            }
            let mut note =
                NewEvent::new(committee, Kind::Note, Address::Speaker, report_body.clone())
                    .with_meta(meta::CONFERENCE_ID, json!(record.id))
                    .with_meta(meta::CONFERENCE_REPORT, payload_json.clone());
            if let Some(r) = recipient {
                note = note.with_to(vec![r]);
            }
            self.commit(note).await?;
        }

        for linked in &record.linked_threads {
            let note = NewEvent::new(
                linked.committee.clone(),
                Kind::Note,
                Address::Speaker,
                report_body.clone(),
            )
            .in_thread(linked.root)
            .with_meta(meta::CONFERENCE_ID, json!(record.id))
            .with_meta(meta::CONFERENCE_REPORT, payload_json.clone());
            self.commit(note).await?;
        }

        Ok(record.summary())
    }
}

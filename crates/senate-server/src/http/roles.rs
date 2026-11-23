//! The roles over HTTP: the chair, the Speaker's desk, secretaries, child
//! Senates, and the rules an append follows across a committee's boundary.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use senate_core::api::{
    ChairInfo, ChildSeats, ChildSenate, RegisterChild, Resolved, Secretaries, SetChair,
    SetSecretaries,
};
use senate_core::{
    Address, Appended, CommitteeId, DocketPage, Hub, Kind, Locator, Name, NewEvent, PickPayload,
    ResolveError, Roster, SenatePath, SenatorName, check_not_keyword, meta,
};
use serde::Deserialize;

use super::{
    ApiFailure, AppState, Person, Sender, chair_discussion, committee_path, json_body, query,
};

/// Keys only the Speaker's office (or a child's) writes: who acted for it,
/// where a forwarded message started, where it was forwarded, who answered
/// it there. A senator carrying one could pass a message off as the
/// desk's, or have the server relay a "Speaker's" answer into another
/// committee's thread.
const OFFICE_KEYS: [&str; 4] = [
    meta::ACTED_BY,
    meta::ORIGIN,
    meta::FORWARDED_TO,
    meta::ANSWERED_BY,
];

pub(super) fn reject_office_keys(event: &NewEvent, sender: Sender) -> Result<(), ApiFailure> {
    if sender != Sender::Senator {
        return Ok(());
    }
    // A senator's escalation points back at the thread it is about, on its
    // own committee: an origin there is no claim to speak for the office.
    let own_thread = senate_core::Origin::of_new(event).is_some_and(|origin| {
        origin.locator.committee() == Some(&event.committee)
            && origin.locator.as_address().is_none()
    });
    match OFFICE_KEYS
        .iter()
        .find(|k| event.meta.contains_key(**k) && !(own_thread && **k == senate_core::meta::ORIGIN))
    {
        Some(key) => Err(ApiFailure::forbidden(format!(
            "meta.{key} is written by the Speaker's office, not by senators"
        ))),
        None => Ok(()),
    }
}

/// `from: <child>::<committee>::speaker` is written only by the child's
/// registered Speaker.
pub(super) fn speak_for_child(
    state: &AppState,
    person: &Person,
    senate: &Name,
) -> Result<(), ApiFailure> {
    match state.hub.child(senate) {
        Some(child) if child.speaker == person.0 => Ok(()),
        Some(child) => Err(ApiFailure::forbidden(format!(
            "{} speaks for the child Senate {senate}; {} does not",
            Address::senator(&child.speaker),
            person.0
        ))),
        None => Err(ApiFailure::forbidden(format!(
            "{senate} is not a child Senate registered here"
        ))),
    }
}

/// A child Senate's office does not decide (people decide) and runs no
/// work, as the Speaker's office does not.
pub(super) fn check_child_write(event: &NewEvent) -> Result<(), ApiFailure> {
    if event.kind == Kind::Decision {
        return Err(ApiFailure::forbidden(
            "a Speaker's office does not decide; people decide",
        ));
    }
    if event.meta.contains_key(meta::WORK) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is the built-in session's",
            meta::WORK
        )));
    }
    Ok(())
}

/// `to: <child>::<committee>::speaker` reaches only a child Senate
/// registered here.
pub(super) fn check_children_addressed(
    state: &AppState,
    event: &NewEvent,
) -> Result<(), ApiFailure> {
    for address in &event.to {
        if let Some((senate, _)) = address.as_child()
            && state.hub.child(senate).is_none()
        {
            return Err(ApiFailure::bad_request(format!(
                "{address}: {senate} is not a child Senate registered here"
            )));
        }
    }
    Ok(())
}

/// The child Senate registered here that `locator` names, if any:
/// `north-desk::ledger::...`, or this Senate's path and one name more.
fn child_named(state: &AppState, path: &SenatePath, locator: &Locator) -> Option<ChildSenate> {
    let senate = locator.senate()?;
    let relative = !senate.is_absolute() && senate.depth() == 1;
    let below = senate.depth() == path.depth() + 1 && senate.names().starts_with(path.names());
    (relative || below).then(|| state.hub.child(senate.name()))?
}

/// A message carrying [`meta::INTENDED`] (a qualified name, as a tool
/// writes it) is meant for a seat outside the committee it is written on.
///
/// - Here, on this committee: addressed to it.
/// - Here, on another committee: a person's message (and the Speaker's
///   office's, or a child's) is delivered there directly, a new root
///   addressed to it; any other sender's (an agent's, a team's or a
///   service's) stays on its own committee addressed to the Speaker's
///   desk, which forwards it, refuses it, or leaves it for the Speaker.
///   An agent's question or ordinary note to the other committee's chair
///   opens a discussion directly in the two committees.
/// - In a child Senate registered here: addressed to that child's
///   Speaker's office for the committee, `<child>::<committee>::speaker`.
/// - Elsewhere (above, or a Senate unknown here): an agent's goes to the
///   desk; a person's is refused with [`ELSEWHERE`](senate_core::api::code::ELSEWHERE): their own
///   machine posts it to the Senate above.
///
/// `meta.intended` is kept, completed to the qualified name it resolved to.
pub(super) async fn append_routed(
    state: &AppState,
    roster: &Roster,
    sender: Sender,
    mut event: NewEvent,
) -> Result<Appended, ApiFailure> {
    let Some(raw) = event.meta.get(meta::INTENDED) else {
        if sender == Sender::Senator
            && let Some(peer) = chair_discussion::reply_peer(&state.hub, &event).await?
        {
            return chair_discussion::reply(&state.hub, event, peer).await;
        }
        return append_local(state, roster, event).await;
    };
    let intended: Locator = raw
        .as_str()
        .ok_or_else(|| {
            ApiFailure::bad_request(format!("meta.{} is a qualified name", meta::INTENDED))
        })?
        .parse()
        .map_err(|e| ApiFailure::bad_request(format!("meta.{}: {e}", meta::INTENDED)))?;
    let direct = match sender {
        Sender::Speaker | Sender::Child => true,
        Sender::Senator => event
            .from
            .as_senator()
            .and_then(|s| roster.principal(s.name()))
            .is_some_and(|p| p.kind.is_person()),
    };
    let path = state.hub.senate_path();
    let set_intended = |event: &mut NewEvent, locator: &Locator| {
        event.meta.insert(
            meta::INTENDED.into(),
            serde_json::Value::String(locator.to_string()),
        );
    };
    let to_desk = |mut event: NewEvent, locator: &Locator| {
        if state.hub.office().is_none() {
            return Err(ApiFailure::conflict(format!(
                "{} is outside this committee, and this Senate has no Speaker whose desk could take it",
                locator.in_text()
            )));
        }
        event.to = vec![Address::Speaker];
        set_intended(&mut event, locator);
        Ok(event)
    };

    if let Some(child) = child_named(state, &path, &intended) {
        let committee = intended
            .committee()
            .cloned()
            .unwrap_or_else(|| event.committee.clone());
        let child_path = path
            .child(child.name.clone())
            .map_err(|e| ApiFailure::bad_request(e.to_string()))?;
        let completed = Locator::at(child_path, committee.clone(), intended.address().clone());
        event.to = vec![Address::child(child.name, committee)];
        set_intended(&mut event, &completed);
        return append_local(state, roster, event).await;
    }

    let event = match state.hub.resolve(&intended, Some(&event.committee)).await? {
        Resolved::Here {
            locator,
            committee,
            address,
        } => {
            if committee == event.committee {
                event.to = vec![address];
                set_intended(&mut event, &locator);
                Ok(event)
            } else if direct {
                event.committee = committee;
                // A thread is a root's on its own committee: on another
                // one the message is a new root.
                event.thread = None;
                event.to = vec![address];
                set_intended(&mut event, &locator);
                Ok(event)
            } else if address == Address::Chair && chair_discussion::can_start(roster, &event) {
                set_intended(&mut event, &locator);
                return chair_discussion::start(&state.hub, event, committee).await;
            } else {
                to_desk(event, &locator)
            }
        }
        Resolved::Elsewhere if direct => Err(ApiFailure::elsewhere(format!(
            "{} is not in {path} nor in a child Senate registered here: a person's message for it is posted to the Senate above by their own machine",
            intended.in_text()
        ))),
        Resolved::Elsewhere => to_desk(event, &intended),
        Resolved::Ambiguous { candidates } => Err(ApiFailure::ambiguous_name(
            format!("{} names several seats here", intended.in_text()),
            candidates,
        )),
        Resolved::Unknown { reason } => Err(ApiFailure::bad_request(reason)),
    }?;
    append_local(state, roster, event).await
}

async fn append_local(
    state: &AppState,
    roster: &Roster,
    event: NewEvent,
) -> Result<Appended, ApiFailure> {
    let event = put_to_pick(state, roster, event).await?;
    Ok(state.hub.resolve_and_append(event).await?)
}

/// A sender that cannot be expected to know the agents (anyone but an
/// agent or the Speaker's office) wrote `senator::*`, and several agents
/// are on the committee. The message is put to someone to pick
/// ([`meta::PICK`] naming the candidates): to the committee's chair when
/// the chair is an agent connected on the committee; else to the
/// Speaker's desk when anyone works it; else it is refused with the
/// candidates (409), as an agent sender always is. One pick per message:
/// several ambiguous wildcards in one are refused too.
async fn put_to_pick(
    state: &AppState,
    roster: &Roster,
    mut event: NewEvent,
) -> Result<NewEvent, ApiFailure> {
    if roster.is_agent_address(&event.from)
        || event.from == Address::Speaker
        || !event.to.iter().any(Address::is_wildcard)
    {
        return Ok(event);
    }
    let candidates = state.hub.committee_agents(&event.committee).await?;
    let mut pick: Option<PickPayload> = None;
    let mut to = Vec::with_capacity(event.to.len());
    let mut picker_at = None;
    for address in &event.to {
        match roster.resolve_address_among(address, &candidates) {
            Err(ResolveError::Ambiguous { candidates }) if pick.is_none() => {
                pick = Some(PickPayload {
                    owner: None,
                    candidates,
                });
                picker_at = Some(to.len());
            }
            // Refused below, by the ordinary resolution.
            Err(_) => return Ok(event),
            Ok(_) => to.push(address.clone()),
        }
    }
    let (Some(pick), Some(at)) = (pick, picker_at) else {
        return Ok(event);
    };
    let chair = state.hub.chair(&event.committee).await?.chair;
    let chair_present = match &chair {
        Some(chair) if roster.is_agent(chair) => {
            state.hub.is_agent_connected(&event.committee, chair).await
        }
        _ => false,
    };
    let picker = if chair_present {
        Address::Chair
    } else if roster.office().is_some_and(|o| !o.desk.is_empty()) {
        Address::Speaker
    } else {
        return Ok(event);
    };
    if !to.contains(&picker) {
        to.insert(at.min(to.len()), picker);
    }
    event.to = to;
    event.meta.insert(meta::PICK.into(), pick.value());
    Ok(event)
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DocketQuery {
    /// `speaker` (the default): the Speaker's desk. `chair`: what awaits
    /// the committee's chair.
    #[serde(default)]
    role: Option<String>,
}

pub(super) async fn docket(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<DocketQuery>, QueryRejection>,
) -> Result<Json<DocketPage>, ApiFailure> {
    let committee = committee_path(path)?;
    match query(params)?.role.as_deref() {
        None | Some("speaker") => Ok(Json(state.hub.docket(&committee).await?)),
        Some("chair") => Ok(Json(state.hub.chair_docket(&committee).await?)),
        Some(other) => Err(ApiFailure::bad_request(format!(
            "role is speaker or chair, not {other:?}"
        ))),
    }
}

pub(super) async fn get_chair(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<ChairInfo>, ApiFailure> {
    let committee = committee_path(path)?;
    Ok(Json(state.hub.chair(&committee).await?))
}

/// A principal of the Senate sets a committee's chair (the Speaker's
/// office over HTTP is a principal's session too).
pub(super) async fn put_chair(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<ChairAuthority>, QueryRejection>,
    body: Result<Json<SetChair>, JsonRejection>,
) -> Result<Json<ChairInfo>, ApiFailure> {
    let committee: CommitteeId = committee_path(path)?;
    let SetChair { senator } = json_body(body)?;
    let roster = state.hub.roster_now();
    let Some(principal) = roster.principal(&person.0) else {
        return Err(ApiFailure::forbidden(format!(
            "{} is not a principal of this Senate: a principal or the Speaker's office sets a chair",
            person.0
        )));
    };
    let by = match query(params)?.acted_by {
        Some(actor) => {
            if !roster.may_act_for_office(&person.0, &actor)
                || roster
                    .office()
                    .is_some_and(|office| office.built_in.as_ref() == Some(&actor))
            {
                return Err(ApiFailure::forbidden(format!(
                    "{} may not set the chair as {}: only the Speaker or its secretary may act for the office",
                    person.0,
                    Address::senator(&actor)
                )));
            }
            actor
        }
        None => principal.path(),
    };
    Ok(Json(
        state
            .hub
            .set_chair_as(&committee, senator, Address::senator(&by))
            .await?,
    ))
}

#[derive(Deserialize)]
pub(super) struct ChairAuthority {
    acted_by: Option<SenatorName>,
}

pub(super) async fn secretaries(
    State(state): State<AppState>,
    _person: Person,
) -> Result<Json<Secretaries>, ApiFailure> {
    Ok(Json(state.hub.secretaries()))
}

/// A senator's secretaries are registered by the senator itself, by the
/// principal that runs it, or by the Speaker's office (naming who acts).
pub(super) async fn put_secretaries(
    method: axum::http::Method,
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<SetSecretaries>, JsonRejection>,
) -> Result<Json<Secretaries>, ApiFailure> {
    let Path(raw) = path.map_err(|e| ApiFailure::bad_request(e.body_text()))?;
    let senator: SenatorName = raw
        .parse()
        .map_err(|e: senate_core::SenatorNameError| ApiFailure::bad_request(e.to_string()))?;
    let senator = state.hub.primary_name(&senator, None).await?;
    let SetSecretaries {
        mut secretaries,
        acted_by,
    } = json_body(body)?;
    let roster = state.hub.roster_now();
    for name in &mut secretaries {
        *name = state.hub.primary_name(name, None).await?;
    }
    let itself = senator == person.0;
    let runs_it = roster.owner_of(&senator) == Some(&person.0);
    let office = acted_by.as_ref().is_some_and(|acted_by| {
        state.hub.built_in() != Some(acted_by) && roster.may_act_for_office(&person.0, acted_by)
    });
    if !(itself || runs_it || office) {
        return Err(ApiFailure::forbidden(format!(
            "{} may not register secretaries for {}: the senator itself, the principal that runs it, or the Speaker's office does",
            person.0,
            Address::senator(&senator)
        )));
    }
    // A secretary not seated yet (a daemon-provided one, say) is seated now,
    // run by whoever registers it, when they may act as it.
    for secretary in &secretaries {
        let fresh = !roster.knows(secretary) && state.hub.built_in() != Some(secretary);
        if fresh {
            state
                .hub
                .act_as(&person.0, secretary)
                .await
                .map_err(|e| ApiFailure::forbidden(e.to_string()))?;
        }
    }
    let result = if method == axum::http::Method::POST {
        state.hub.add_secretaries(senator, secretaries).await?
    } else {
        state.hub.set_secretaries(senator, secretaries).await?
    };
    Ok(Json(result))
}

pub(super) async fn children(
    State(state): State<AppState>,
    _person: Person,
) -> Result<Json<Vec<ChildSenate>>, ApiFailure> {
    Ok(Json(state.hub.children()))
}

fn child_name(path: Result<Path<String>, PathRejection>) -> Result<Name, ApiFailure> {
    let Path(raw) = path.map_err(|e| ApiFailure::bad_request(e.body_text()))?;
    let name = Name::new(&raw).map_err(|e| ApiFailure::bad_request(e.to_string()))?;
    check_not_keyword(name.as_str(), "a Senate")
        .map_err(|e| ApiFailure::bad_request(e.to_string()))?;
    Ok(name)
}

pub(super) async fn get_child(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<ChildSenate>, ApiFailure> {
    let name = child_name(path)?;
    state
        .hub
        .child(&name)
        .map(Json)
        .ok_or_else(|| ApiFailure::not_found(format!("no child Senate {name} is registered here")))
}

/// A registered child declares its seats, authenticated as its Speaker:
/// the whole set, replacing the last one, and the child is seen now.
pub(super) async fn put_child_seats(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<ChildSeats>, JsonRejection>,
) -> Result<Json<ChildSenate>, ApiFailure> {
    let name = child_name(path)?;
    let ChildSeats { seats } = json_body(body)?;
    let Some(child) = state.hub.child(&name) else {
        return Err(ApiFailure::not_found(format!(
            "no child Senate {name} is registered here; register it first"
        )));
    };
    if child.speaker != person.0 {
        return Err(ApiFailure::forbidden(format!(
            "{} may not declare the seats of {name}: its Speaker {} does",
            person.0,
            Address::senator(&child.speaker)
        )));
    }
    state
        .hub
        .set_child_seats(&name, seats)
        .map(Json)
        .ok_or_else(|| ApiFailure::not_found(format!("no child Senate {name} is registered here")))
}

/// A child Senate registers authenticated as its Speaker: `speaker` is
/// `/<this Senate's path>::senator::<x>`, `x` a principal here and the one
/// who authenticated. A child whose Speaker would be an agent of this
/// Senate is refused: not supported yet.
pub(super) async fn put_child(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<RegisterChild>, JsonRejection>,
) -> Result<Json<ChildSenate>, ApiFailure> {
    let name = child_name(path)?;
    let RegisterChild { speaker } = json_body(body)?;
    if &name == state.hub.senate_name() {
        return Err(ApiFailure::bad_request(format!(
            "{name} is this Senate's own name; a child Senate is named otherwise"
        )));
    }
    let here = state.hub.senate_path();
    if !speaker.senate().is_some_and(|s| s.same_names(&here)) {
        return Err(ApiFailure::bad_request(format!(
            "a child's Speaker is a senator of this Senate, written {here}::senator::<name>; not {speaker}"
        )));
    }
    let roster = state.hub.roster_now();
    let x = speaker.name();
    if roster.is_agent(x) {
        return Err(ApiFailure::forbidden(format!(
            "{speaker} is an agent: an agent as a child Senate's Speaker is not supported yet"
        )));
    }
    if roster.principal(x.name()).is_none() {
        return Err(ApiFailure::bad_request(format!(
            "{speaker} is not on this Senate's roster: a child's Speaker is one of its principals"
        )));
    }
    if x != &person.0 {
        return Err(ApiFailure::forbidden(format!(
            "{} may not register a child Senate whose Speaker is {speaker}: the Speaker registers it",
            person.0
        )));
    }
    if let Some(existing) = state.hub.child(&name)
        && existing.speaker != *x
    {
        return Err(ApiFailure::conflict(format!(
            "the child Senate {name} is already registered with the Speaker {}",
            Address::senator(&existing.speaker)
        )));
    }
    let child = ChildSenate::new(name, x.clone());
    Ok(Json(state.hub.register_child(child).await?))
}

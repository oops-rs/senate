use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use senate_core::api::{
    CommitteeArchiveInfo, CommitteeSeatInfo, CommitteeSeatStatus, SetCommitteeArchive,
    SetCommitteeSeat,
};
use senate_core::{Address, CommitteeArchive, CommitteeSeatChange, SenatorKind, SenatorName};
use serde::Deserialize;

use super::{ApiFailure, AppState, Person, committee_path, json_body, query};

#[derive(Deserialize)]
pub(super) struct SeatQuery {
    senator: SenatorName,
}

pub(super) async fn get_seat(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<SeatQuery>, QueryRejection>,
) -> Result<Json<CommitteeSeatStatus>, ApiFailure> {
    let committee = committee_path(path)?;
    let senator = state
        .hub
        .primary_name(&query(params)?.senator, Some(&committee))
        .await?;
    let unseated = state
        .hub
        .is_explicitly_unseated(&committee, &senator)
        .await?;
    Ok(Json(CommitteeSeatStatus { unseated }))
}

pub(super) async fn put_seat(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<SetCommitteeSeat>, JsonRejection>,
) -> Result<Json<CommitteeSeatInfo>, ApiFailure> {
    let committee = committee_path(path)?;
    let SetCommitteeSeat {
        senator,
        action,
        id,
        acted_by,
    } = json_body(body)?;
    let senator = state.hub.primary_name(&senator, Some(&committee)).await?;
    let roster = state.hub.roster_now();
    if !roster.is_agent(&senator) {
        return Err(ApiFailure::bad_request(format!(
            "{} must name an agent of this Senate",
            Address::senator(&senator)
        )));
    }
    let owns = roster.owner_of(&senator) == Some(&person.0);
    let office = acted_by.as_ref().is_some_and(|agent| {
        state.hub.built_in() != Some(agent) && roster.may_act_for_office(&person.0, agent)
    });
    if !(owns || office) {
        return Err(ApiFailure::forbidden(format!(
            "{} may change the committee seat of {} only as its owner or for the Speaker's office",
            person.0,
            Address::senator(&senator)
        )));
    }
    let change = CommitteeSeatChange {
        senator,
        action,
        by: person.0,
    };
    Ok(Json(
        state.hub.set_committee_seat(&committee, change, id).await?,
    ))
}

/// Archive or unarchive a committee. Only the Speaker in person: the
/// authenticated person is the Speaker, a person, and no one acts for the
/// office here. (A person's agents hold the person's token too: the
/// machine refuses this request from inside a coding session, which is the
/// rest of the guard.)
pub(super) async fn put_archive(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<SetCommitteeArchive>, JsonRejection>,
) -> Result<Json<CommitteeArchiveInfo>, ApiFailure> {
    let committee = committee_path(path)?;
    let SetCommitteeArchive { archived, id } = json_body(body)?;
    let speaker_in_person = state.hub.roster_now().office().is_some_and(|office| {
        office.kind == SenatorKind::Person && office.speaker.name().name() == &person.0
    });
    if !speaker_in_person {
        return Err(ApiFailure::forbidden(format!(
            "only the Speaker in person archives or unarchives a committee; {} is not",
            person.0
        )));
    }
    let known = state.hub.committees_by_archive().await?;
    if !known.0.contains(&committee) && !known.1.contains(&committee) {
        return Err(ApiFailure::bad_request(format!(
            "unknown committee {committee}"
        )));
    }
    let change = CommitteeArchive {
        archived,
        by: person.0,
    };
    Ok(Json(
        state
            .hub
            .set_committee_archive(&committee, change, id)
            .await?,
    ))
}

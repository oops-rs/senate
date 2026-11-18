use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use senate_core::api::{CommitteeSeatInfo, CommitteeSeatStatus, SetCommitteeSeat};
use senate_core::{Address, CommitteeSeatChange, SenatorName};
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
    let senator = state.hub.primary_name(&query(params)?.senator).await;
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
    let senator = state.hub.primary_name(&senator).await;
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

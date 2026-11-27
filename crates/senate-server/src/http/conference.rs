//! HTTP + SSE endpoints for `/v1/conferences*`.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response, Sse};
use senate_core::api::StreamQuery;
use senate_core::conference::{
    AdjournConferenceRequest, ConferenceDetail, ConferenceId, ConferenceList, ConferenceRsvpAction,
    ConferenceSummary, CreateConferenceRequest, PostConferenceEventRequest,
    PostConferenceEventResponse,
};

use super::{
    ApiFailure, AppState, Person, json_body, last_event_id, query, reject_cursor_ahead, sse,
};
use crate::hub::ConferenceError;

impl From<ConferenceError> for ApiFailure {
    fn from(error: ConferenceError) -> Self {
        match error {
            ConferenceError::BadRequest(msg) => Self::bad_request(msg),
            ConferenceError::Forbidden(msg) => Self::forbidden(msg),
            ConferenceError::NotFound(msg) => Self::not_found(msg),
            ConferenceError::Conflict(msg) => Self::conflict(msg),
            ConferenceError::Hub(err) => err.into(),
        }
    }
}

fn conference_id(path: Result<Path<String>, PathRejection>) -> Result<ConferenceId, ApiFailure> {
    let Path(raw) = path.map_err(|e| ApiFailure::bad_request(e.body_text()))?;
    ConferenceId::new(raw).map_err(|e| ApiFailure::bad_request(e.to_string()))
}

pub(super) async fn create(
    State(state): State<AppState>,
    person: Person,
    body: Result<Json<CreateConferenceRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ConferenceSummary>), ApiFailure> {
    let summary = state
        .hub
        .convene_conference(&person.0, json_body(body)?)
        .await?;
    Ok((StatusCode::CREATED, Json(summary)))
}

pub(super) async fn list(
    State(state): State<AppState>,
    person: Person,
) -> Result<Json<ConferenceList>, ApiFailure> {
    Ok(Json(state.hub.list_conferences(&person.0).await?))
}

pub(super) async fn get(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<ConferenceDetail>, ApiFailure> {
    let id = conference_id(path)?;
    Ok(Json(state.hub.get_conference(&person.0, &id).await?))
}

pub(super) async fn rsvp(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<ConferenceRsvpAction>, JsonRejection>,
) -> Result<Json<ConferenceSummary>, ApiFailure> {
    let id = conference_id(path)?;
    Ok(Json(
        state
            .hub
            .rsvp_conference(&person.0, &id, json_body(body)?)
            .await?,
    ))
}

pub(super) async fn start(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<ConferenceSummary>, ApiFailure> {
    let id = conference_id(path)?;
    Ok(Json(state.hub.start_conference(&person.0, &id).await?))
}

pub(super) async fn post_event(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<PostConferenceEventRequest>, JsonRejection>,
) -> Result<Json<PostConferenceEventResponse>, ApiFailure> {
    let id = conference_id(path)?;
    Ok(Json(
        state
            .hub
            .post_conference_event(&person.0, &id, json_body(body)?)
            .await?,
    ))
}

pub(super) async fn stream(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<StreamQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    let id = conference_id(path)?;
    let params = query(params)?;
    let agent = state
        .hub
        .authorize_conference_stream(&person.0, &id, params.senator.as_ref())
        .await?;
    let stream_id = id.as_committee_id();
    let after = last_event_id(&headers)?.unwrap_or(params.after);
    reject_cursor_ahead(&state, &stream_id, after).await?;
    let live = state.hub.open_stream(&stream_id, agent.as_ref()).await?;
    let feed = sse::feed(state.hub.clone(), stream_id, live, after, state.ping_every);
    let mut response = Sse::new(feed).into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

pub(super) async fn adjourn(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<AdjournConferenceRequest>, JsonRejection>,
) -> Result<Json<ConferenceSummary>, ApiFailure> {
    let id = conference_id(path)?;
    Ok(Json(
        state
            .hub
            .adjourn_conference(&person.0, &id, json_body(body)?)
            .await?,
    ))
}

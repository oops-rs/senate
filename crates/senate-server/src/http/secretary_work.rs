use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use senate_core::SenatorName;
use senate_core::api::Secretaries;
use senate_core::secretary_work::{
    SecretaryReminder, SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkRequest,
    SecretaryWorkUpdate, SetSecretarySession,
};

use super::{ApiFailure, AppState, Person, json_body};
use crate::hub::SecretaryWorkError;

impl From<SecretaryWorkError> for ApiFailure {
    fn from(error: SecretaryWorkError) -> Self {
        match error {
            SecretaryWorkError::Forbidden(message) => Self::forbidden(message),
            SecretaryWorkError::Conflict(message) => Self::conflict(message),
            SecretaryWorkError::Hub(error) => error.into(),
        }
    }
}

pub(super) async fn put_session(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<SetSecretarySession>, JsonRejection>,
) -> Result<Json<Secretaries>, ApiFailure> {
    let Path(raw) = path.map_err(|error| ApiFailure::bad_request(error.body_text()))?;
    let principal = raw
        .parse::<SenatorName>()
        .map_err(|error| ApiFailure::bad_request(error.to_string()))?;
    Ok(Json(
        state
            .hub
            .set_secretary_session(&person.0, principal, json_body(body)?)
            .await?,
    ))
}

pub(super) async fn work(
    State(state): State<AppState>,
    person: Person,
    body: Result<Json<SecretaryWorkRequest>, JsonRejection>,
) -> Result<Json<SecretaryWorkReply>, ApiFailure> {
    let mut request = json_body(body)?;
    if let SecretaryWorkOp::Change { update, .. } = &mut request.operation {
        let to = match update {
            SecretaryWorkUpdate::Assign { to } => Some(to),
            SecretaryWorkUpdate::Handoff { to, .. } => to.as_mut(),
            _ => None,
        };
        if let Some(to) = to {
            *to = state.hub.primary_name(to).await;
        }
    }
    Ok(Json(state.hub.secretary_work(&person.0, request).await?))
}

pub(super) async fn reminders(
    State(state): State<AppState>,
    person: Person,
    body: Result<Json<Vec<SenatorName>>, JsonRejection>,
) -> Result<Json<Vec<SecretaryReminder>>, ApiFailure> {
    Ok(Json(
        state
            .hub
            .secretary_reminders(&person.0, json_body(body)?)
            .await?,
    ))
}

pub(super) async fn ack_reminder(
    State(state): State<AppState>,
    person: Person,
    body: Result<Json<SecretaryReminder>, JsonRejection>,
) -> Result<StatusCode, ApiFailure> {
    state
        .hub
        .ack_secretary_reminder(&person.0, json_body(body)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

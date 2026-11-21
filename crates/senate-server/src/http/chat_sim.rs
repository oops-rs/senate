//! Simulated chat ([`senate_core::chat_sim`]): who may inject a message,
//! decided here, before the gateway sees it.
//!
//! - A person, as themselves: their token, `by` naming nobody or
//!   themselves, the message from them.
//! - The Speaker's office (`office`), as any person on the roster: who acts
//!   is checked as for any write for the office ([`AppState::preside`]).
//! - Never an agent (`by` naming one the person runs: the bridge names the
//!   coding session that called it), and never as a guest or an agent: the
//!   message is from a person on the roster.

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use senate_core::chat_sim::{SimError, SimRequest, SimTranscript, SimulateChat, SimulatedChat};
use senate_core::{Address, Kind, NewEvent, meta};

use super::{ApiFailure, AppState, Person, committee_path, json_body, query};

impl From<SimError> for ApiFailure {
    fn from(error: SimError) -> Self {
        match error {
            SimError::Invalid(message) => Self::bad_request(message),
            SimError::NotFound(message) => Self::not_found(message),
            SimError::Unavailable(message) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                senate_core::api::code::INTERNAL,
                message,
            ),
        }
    }
}

fn simulator(
    state: &AppState,
) -> Result<std::sync::Arc<dyn senate_core::chat_sim::ChatSimulator>, ApiFailure> {
    state.simulator.clone().ok_or_else(|| {
        ApiFailure::not_found("this Senate has no chat platform: there is no chat to simulate")
    })
}

pub(super) async fn simulate(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<SimulateChat>, JsonRejection>,
) -> Result<Json<SimulatedChat>, ApiFailure> {
    let committee = committee_path(path)?;
    let request = json_body(body)?;
    let roster = state.hub.roster_now();
    if let Some(by) = &request.by {
        roster
            .may_act_as(&person.0, by)
            .map_err(|e| ApiFailure::forbidden(e.to_string()))?;
        if !roster.is_person(by) {
            return Err(ApiFailure::forbidden(format!(
                "{} is an agent: only a person, as themselves, or the Speaker's office may simulate chat",
                Address::senator(by)
            )));
        }
    }
    let from = request
        .as_person
        .clone()
        .unwrap_or_else(|| person.0.clone());
    if request.office {
        // Checked as a write for the office is.
        let mut probe = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, "");
        if let Some(acted_by) = &request.acted_by {
            probe = probe.with_meta(meta::ACTED_BY, serde_json::json!(acted_by.to_string()));
        }
        state.preside(&person, &mut probe)?;
    } else if from != person.0 {
        return Err(ApiFailure::forbidden(format!(
            "{} may simulate only their own messages; the Speaker's office may simulate anyone's",
            person.0
        )));
    }
    if roster.principal(&from).is_none() {
        return Err(ApiFailure::forbidden(format!(
            "{from} is not a person on the roster: only people's messages are simulated, never a guest's or an agent's"
        )));
    }
    let simulator = simulator(&state)?;
    let simulated = simulator
        .simulate(SimRequest {
            committee,
            from,
            text: request.text,
            group: request.group,
            reply_to: request.reply_to,
            ambient: request.ambient,
            live: request.live,
        })
        .await?;
    Ok(Json(simulated))
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct TranscriptQuery {
    #[serde(rename = "ref")]
    message_ref: String,
}

pub(super) async fn transcript(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<TranscriptQuery>, QueryRejection>,
) -> Result<Json<SimTranscript>, ApiFailure> {
    let committee = committee_path(path)?;
    let params = query(params)?;
    let transcript = simulator(&state)?
        .transcript(&committee, &params.message_ref)
        .await?;
    Ok(Json(transcript))
}

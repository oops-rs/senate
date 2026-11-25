//! A separate, deliberately small authorization domain for desk intake.
use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;

use senate_core::worker::{Admin, Identity, Issued, Question, Receipt, Replies, RepliesQuery};
use senate_core::{Address, CommitteeId, EventId, Guest, Hub as _, Kind, Name, NewEvent, Seq};
use senate_log::LogStore;

use super::{ApiFailure, AppState, LocalPerson, bearer_token};

const PREFIX: &str = "desk-worker-grant:";
const MAX_QUESTION_BYTES: usize = 16 * 1024;
const PAGE: usize = 200;

pub(crate) struct Book {
    store: Arc<dyn LogStore>,
    policy: tokio::sync::RwLock<()>,
}

impl Book {
    pub(crate) fn new(store: Arc<dyn LogStore>) -> Self {
        Self {
            store,
            policy: tokio::sync::RwLock::new(()),
        }
    }

    async fn grant(&self, id: &EventId) -> Result<Option<Grant>, ApiFailure> {
        let key = format!("{PREFIX}{id}");
        let rows = self.store.registry(&key).await.map_err(store_error)?;
        rows.into_iter()
            .find(|(found, _)| found == &key)
            .map(|(_, row)| serde_json::from_value(row).map_err(store_error))
            .transpose()
    }

    async fn authorize(&self, headers: &HeaderMap) -> Result<(EventId, Grant), ApiFailure> {
        let raw = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .ok_or_else(ApiFailure::unauthorized)?;
        let (id, secret) = raw
            .strip_prefix("sw1.")
            .and_then(|raw| raw.split_once('.'))
            .ok_or_else(ApiFailure::unauthorized)?;
        if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiFailure::unauthorized());
        }
        let id = id.parse().map_err(|_| ApiFailure::unauthorized())?;
        let grant = self
            .grant(&id)
            .await?
            .ok_or_else(ApiFailure::unauthorized)?;
        let digest: [u8; 32] = Sha256::digest(raw.as_bytes()).into();
        if !bool::from(digest.ct_eq(&grant.digest)) {
            return Err(ApiFailure::unauthorized());
        }
        Ok((id, grant))
    }
}

#[derive(Serialize, Deserialize)]
struct Grant {
    worker: Name,
    committees: BTreeSet<CommitteeId>,
    digest: [u8; 32],
}

fn store_error(_error: impl std::fmt::Display) -> ApiFailure {
    ApiFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "desk-worker credential storage failed",
    )
}

fn book(state: &AppState) -> Result<&Book, ApiFailure> {
    state
        .workers
        .as_deref()
        .ok_or_else(|| ApiFailure::not_found("desk-worker API is machine-only"))
}

fn worker_address(id: &EventId) -> Result<Address, ApiFailure> {
    Ok(Address::Guest(
        Guest::new("desk-worker", &id.to_string()).map_err(store_error)?,
    ))
}

fn permitted(grant: &Grant, committee: &CommitteeId) -> Result<(), ApiFailure> {
    if grant.committees.contains(committee) {
        Ok(())
    } else {
        Err(ApiFailure::forbidden(
            "the desk-worker grant does not include this committee",
        ))
    }
}

pub(super) async fn admin(
    State(state): State<AppState>,
    local: Option<Extension<LocalPerson>>,
    Json(request): Json<Admin>,
) -> Result<Json<Option<Issued>>, ApiFailure> {
    if local.is_none() {
        return Err(ApiFailure::not_found("no such route"));
    }
    let book = book(&state)?;
    let _write = book.policy.write().await;
    match request {
        Admin::Issue { worker, committees } => {
            let roster = state.hub.roster().await?;
            let name = senate_core::SenatorName::new(worker.clone())
                .map_err(|_| ApiFailure::bad_request("the worker label is reserved"))?;
            if roster.principal(&worker).is_some() || roster.is_agent(&name) {
                return Err(ApiFailure::bad_request(
                    "the worker label must differ from person and senator names",
                ));
            }
            let known: BTreeSet<_> = state.hub.committees().await?.into_iter().collect();
            if committees.is_empty() || !committees.is_subset(&known) {
                return Err(ApiFailure::bad_request(
                    "choose an explicit nonempty allowlist of existing committees",
                ));
            }
            let id = EventId::new();
            let mut secret = [0u8; 32];
            getrandom::fill(&mut secret).map_err(store_error)?;
            let token = format!("sw1.{id}.{}", hex::encode(secret));
            let grant = Grant {
                worker,
                committees,
                digest: Sha256::digest(token.as_bytes()).into(),
            };
            let value = serde_json::to_value(grant).map_err(store_error)?;
            book.store
                .put_registry(&format!("{PREFIX}{id}"), Some(value))
                .await
                .map_err(store_error)?;
            Ok(Json(Some(Issued { id, token })))
        }
        Admin::Revoke { id } => {
            if book.grant(&id).await?.is_none() {
                return Err(ApiFailure::not_found("no such desk-worker credential"));
            }
            book.store
                .put_registry(&format!("{PREFIX}{id}"), None)
                .await
                .map_err(store_error)?;
            Ok(Json(None))
        }
    }
}

pub(super) async fn identity(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Identity>, ApiFailure> {
    let book = book(&state)?;
    let _read = book.policy.read().await;
    let (_, grant) = book.authorize(&headers).await?;
    Ok(Json(Identity {
        worker: grant.worker,
        senate: state.hub.senate_path().name().clone(),
        epoch: state.hub.epoch().await?,
        committees: grant.committees,
        office: state.hub.roster().await?.office(),
    }))
}

fn caller_label(raw: &str) -> Option<String> {
    let filtered: String = raw
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, ' ' | '-' | '_' | '.' | '@') {
                ch
            } else {
                ' '
            }
        })
        .take(96)
        .collect();
    let label = filtered.split_whitespace().collect::<Vec<_>>().join(" ");
    (!label.is_empty()).then_some(label)
}

pub(super) async fn question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<Question>,
) -> Result<Json<Receipt>, ApiFailure> {
    let book = book(&state)?;
    let _read = book.policy.read().await;
    let (credential, grant) = book.authorize(&headers).await?;
    permitted(&grant, &request.committee)?;
    if request.question.trim().is_empty() || request.question.len() > MAX_QUESTION_BYTES {
        return Err(ApiFailure::bad_request(
            "question must contain 1–16384 UTF-8 bytes",
        ));
    }
    let caller = request.caller.as_deref().and_then(caller_label);
    let heading = match &caller {
        Some(label) => format!("Asked through {} on behalf of {}:", grant.worker, label),
        None => format!("Asked through {}:", grant.worker),
    };
    let authored = worker_address(&credential)?;
    let event = NewEvent::new(
        request.committee.clone(),
        Kind::Question,
        authored.clone(),
        format!("{heading}\n\n{}", request.question),
    )
    .to(Address::Speaker)
    .with_id(request.id.clone());
    let expected_body = event.body.clone();
    let stored = state.hub.append(event).await?.event;
    if stored.id != request.id
        || stored.committee != request.committee
        || stored.from != authored
        || stored.kind != Kind::Question
        || stored.body != expected_body
        || stored.to != [Address::Speaker]
        || stored.thread.is_some()
    {
        return Err(ApiFailure::conflict(
            "this request ID belongs to a different question",
        ));
    }
    Ok(Json(Receipt {
        id: stored.id,
        seq: stored.seq,
        worker: grant.worker,
        caller,
    }))
}

pub(super) async fn replies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RepliesQuery>,
) -> Result<Json<Replies>, ApiFailure> {
    let book = book(&state)?;
    let _read = book.policy.read().await;
    let (credential, grant) = book.authorize(&headers).await?;
    permitted(&grant, &query.committee)?;
    if query.root == Seq::ZERO {
        return Err(ApiFailure::not_found("no such worker thread"));
    }
    let owner = worker_address(&credential)?;
    let root = state
        .hub
        .read(&query.committee, Seq(query.root.0 - 1), 1)
        .await?
        .into_iter()
        .next()
        .filter(|event| {
            event.seq == query.root
                && event.from == owner
                && event.kind == Kind::Question
                && event.to == [Address::Speaker]
                && event.thread.is_none()
        });
    if root.is_none() {
        return Err(ApiFailure::not_found("no such worker thread"));
    }
    let after = query.after.max(query.root);
    let events = state.hub.read(&query.committee, after, PAGE).await?;
    let next = events.last().map_or(after, |event| event.seq);
    let office = state.hub.roster().await?.office();
    let events = events
        .into_iter()
        .filter(|event| {
            event.kind == Kind::Answer
                && event.thread == Some(query.root)
                && (event.from == Address::Speaker
                    || event.from.as_senator().is_some_and(|sender| {
                        office.as_ref().is_some_and(|office| {
                            office.agent_workers().any(|worker| worker == sender)
                        })
                    }))
        })
        .collect();
    Ok(Json(Replies { events, next }))
}

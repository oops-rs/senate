//! The HTTP/SSE API, exactly as `senate_core::api` documents it.

mod chair_discussion;
mod committee_seat;
mod error;
mod roles;
mod secretary_work;
mod sse;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::map_response_with_state;
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use senate_core::api::{
    AppendResponse, CheckpointQuery, Cursor, CursorQuery, DEFAULT_PAGE, DEFAULT_SEARCH,
    EPOCH_HEADER, EventsPage, EventsQuery, Heartbeat, MAX_PAGE, MAX_SEARCH, Me, PresencePage,
    ResolveQuery, Resolved, SearchQuery, SearchResults, StreamQuery, ThreadsPage, ThreadsQuery,
};
use senate_core::{
    Address, Brief, CheckpointPayload, CheckpointRecord, CommitteeId, DutyPayload, Hub, Kind, Name,
    NewEvent, Roster, SenatorName, Seq, StatusPayload, StatusState, meta,
};

use crate::auth::{Authenticate, bearer_token};
use crate::hub::ServerHub;
use crate::rules::SPEAKER_KEYS;
pub(crate) use error::ApiFailure;

#[derive(Clone)]
pub(crate) struct AppState {
    pub hub: Arc<ServerHub>,
    pub auth: Arc<dyn Authenticate>,
    /// The log's epoch, stamped on every response.
    pub epoch: HeaderValue,
    pub ping_every: Duration,
    /// Whether a Speaker judges task roots here.
    pub triaged: bool,
}

pub(crate) fn router(state: AppState) -> Router {
    let epoch = state.epoch.clone();
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/me", get(me))
        .route("/v1/aliases", get(list_aliases))
        .route("/v1/aliases/{primary}", put(set_alias))
        .route("/v1/aliases/{primary}/{alias}", delete(remove_alias))
        .route(
            "/v1/committees/{committee}/events",
            post(append).get(events),
        )
        .route("/v1/committees/{committee}/stream", get(stream))
        .route(
            "/v1/committees/{committee}/cursors/{name}",
            get(get_cursor).put(put_cursor),
        )
        .route("/v1/committees/{committee}/heartbeat", post(heartbeat))
        .route(
            "/v1/committees/{committee}/seat",
            get(committee_seat::get_seat).put(committee_seat::put_seat),
        )
        .route("/v1/committees/{committee}/search", get(search))
        .route("/v1/committees/{committee}/brief", get(brief))
        .route("/v1/committees/{committee}/threads", get(threads))
        .route("/v1/committees/{committee}/presence", get(presence))
        .route(
            "/v1/committees/{committee}/checkpoints/latest",
            get(latest_checkpoint),
        )
        .route("/v1/committees/{committee}/docket", get(roles::docket))
        .route(
            "/v1/committees/{committee}/chair",
            get(roles::get_chair).put(roles::put_chair),
        )
        .route("/v1/secretaries", get(roles::secretaries))
        .route("/v1/secretaries/{senator}", put(roles::put_secretaries))
        .route(
            "/v1/secretary-teams/{person}/session",
            put(secretary_work::put_session),
        )
        .route(
            senate_core::api::SECRETARY_WORK_PATH,
            post(secretary_work::work),
        )
        .route(
            senate_core::api::SECRETARY_REMINDERS_PATH,
            post(secretary_work::reminders).put(secretary_work::ack_reminder),
        )
        .route("/v1/children", get(roles::children))
        .route(
            "/v1/children/{name}",
            get(roles::get_child).put(roles::put_child),
        )
        .route("/v1/children/{name}/seats", put(roles::put_child_seats))
        .route("/v1/resolve", get(resolve))
        .fallback(|| async { ApiFailure::not_found("no such route") })
        .method_not_allowed_fallback(|| async { ApiFailure::method_not_allowed() })
        // Outermost, so errors and fallbacks carry it too.
        .layer(map_response_with_state(epoch, stamp_epoch))
        .with_state(state)
}

/// Every response names the log's epoch ([`EPOCH_HEADER`]).
async fn stamp_epoch(State(epoch): State<HeaderValue>, mut response: Response) -> Response {
    response.headers_mut().insert(EPOCH_HEADER, epoch);
    response
}

/// The authenticated person. A token never names an agent; what the person
/// does as an agent names it in the request (see [`AppState::act_as`]).
struct Person(Name);

impl FromRequestParts<AppState> for Person {
    type Rejection = ApiFailure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .ok_or_else(ApiFailure::unauthorized)?;
        state
            .auth
            .authenticate(token)
            .await
            .map(Person)
            .ok_or_else(ApiFailure::unauthorized)
    }
}

impl AppState {
    /// `person` acts as `senator`: themselves, or an agent under their own
    /// name that could sit in this Senate, which is seated if this is its
    /// first contact. Anyone else's, and the built-in session, is 403.
    async fn act_as(&self, person: &Person, senator: &SenatorName) -> Result<(), ApiFailure> {
        self.hub
            .act_as(&person.0, senator)
            .await
            .map(|_| ())
            .map_err(|e| ApiFailure::forbidden(e.to_string()))
    }

    /// `person` writes for the Speaker's office (`from: speaker`), naming
    /// who acts in [`meta::ACTED_BY`]: the Speaker itself (a principal
    /// Speaker, or an agent Speaker `person` runs) or one of the Speaker's
    /// secretaries that `person` runs. With no `acted_by` and exactly one
    /// desk worker run by `person`, it is that one, stamped on the event.
    /// The built-in session writes in process; nobody acts as it here.
    fn preside(&self, person: &Person, event: &mut NewEvent) -> Result<(), ApiFailure> {
        let roster = self.hub.roster_now();
        let Some(office) = roster.office() else {
            return Err(ApiFailure::forbidden(
                "this Senate has no Speaker; nobody writes as the Speaker",
            ));
        };
        let speaker = office.speaker.clone();
        let acted_by = match event.meta.get(meta::ACTED_BY) {
            Some(value) => value
                .as_str()
                .and_then(|s| s.parse::<SenatorName>().ok())
                .ok_or_else(|| {
                    ApiFailure::bad_request(format!(
                        "meta.{} names the senator acting for the Speaker's office, as a name",
                        meta::ACTED_BY
                    ))
                })?,
            None => {
                let mine: Vec<&SenatorName> = office
                    .agent_workers()
                    .filter(|w| roster.owner_of(w) == Some(&person.0))
                    .collect();
                match mine.as_slice() {
                    [one] => (*one).clone(),
                    _ => {
                        return Err(ApiFailure::forbidden(format!(
                            "{} writes for the office of the Speaker {speaker} only naming who acts in meta.{}: the Speaker, or one of the Speaker's secretaries {} runs",
                            person.0,
                            meta::ACTED_BY,
                            person.0
                        )));
                    }
                }
            }
        };
        if office.built_in.as_ref() == Some(&acted_by)
            || !roster.may_act_for_office(&person.0, &acted_by)
        {
            return Err(ApiFailure::forbidden(format!(
                "{} may not act for the office of the Speaker {speaker} as {}: only the Speaker, or a secretary of the Speaker that {} runs",
                person.0,
                Address::senator(&acted_by),
                person.0
            )));
        }
        event.meta.insert(
            meta::ACTED_BY.into(),
            serde_json::Value::String(acted_by.to_string()),
        );
        Ok(())
    }

    /// Like [`AppState::act_as`], for a connection rather than a write: a
    /// stream, a cursor, a heartbeat. Besides the senators `person` may act
    /// as, an agent working the Speaker's desk that `person` runs: the
    /// session working the desk follows this Senate's log with its own
    /// cursors and presence. Should the roster refuse it, it is still let
    /// in.
    async fn connect_as(&self, person: &Person, senator: &SenatorName) -> Result<(), ApiFailure> {
        let roster = self.hub.roster_now();
        let works_desk = roster.office().is_some_and(|office| {
            office.agent_workers().any(|w| w == senator)
                && roster.owner_of(senator) == Some(&person.0)
        });
        match self.act_as(person, senator).await {
            Err(_) if works_desk => Ok(()),
            result => result,
        }
    }

    /// Like [`AppState::connect_as`], for a request that may name no
    /// senator: it is then the person's own.
    async fn act_as_or_self(
        &self,
        person: &Person,
        senator: Option<SenatorName>,
    ) -> Result<SenatorName, ApiFailure> {
        let senator = match senator {
            Some(senator) => senator,
            None => SenatorName::new(person.0.clone())
                .map_err(|e| ApiFailure::forbidden(e.to_string()))?,
        };
        self.connect_as(person, &senator).await?;
        Ok(senator)
    }
}

fn committee_id(raw: &str) -> Result<CommitteeId, ApiFailure> {
    CommitteeId::new(raw).map_err(|e| ApiFailure::bad_request(e.to_string()))
}

fn committee_path(path: Result<Path<String>, PathRejection>) -> Result<CommitteeId, ApiFailure> {
    let Path(raw) = path.map_err(|e| ApiFailure::bad_request(e.body_text()))?;
    committee_id(&raw)
}

/// Cursor names are short slugs: `delivered`, `read`, ...
fn cursor_name(raw: &str) -> Result<&str, ApiFailure> {
    let valid = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(raw)
    } else {
        Err(ApiFailure::bad_request(format!(
            "cursor name must be 1-64 chars of [A-Za-z0-9_-]: {raw:?}"
        )))
    }
}

fn cursor_path(
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<(CommitteeId, String), ApiFailure> {
    let Path((committee, name)) = path.map_err(|e| ApiFailure::bad_request(e.body_text()))?;
    let name = cursor_name(&name)?.to_string();
    Ok((committee_id(&committee)?, name))
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiFailure> {
    body.map(|Json(value)| value)
        .map_err(|e| ApiFailure::bad_request(e.body_text()))
}

fn query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiFailure> {
    query
        .map(|Query(value)| value)
        .map_err(|e| ApiFailure::bad_request(e.body_text()))
}

async fn list_aliases(
    State(state): State<AppState>,
    _person: Person,
) -> Json<senate_core::api::SenatorAliases> {
    Json(state.hub.aliases().await)
}

async fn set_alias(
    State(state): State<AppState>,
    person: Person,
    Path(primary): Path<SenatorName>,
    body: Result<Json<senate_core::api::SetAlias>, JsonRejection>,
) -> Result<Json<SenatorName>, ApiFailure> {
    let request = json_body(body)?;
    if !primary.is_session_name() || primary == person.0 {
        return Err(ApiFailure::bad_request(
            "an alias needs a coding session's primary senator ID",
        ));
    }
    state
        .hub
        .roster_now()
        .may_act_as(&person.0, &primary)
        .map_err(|error| ApiFailure::forbidden(error.to_string()))?;
    Ok(Json(
        state.hub.set_alias(&person.0, &primary, request).await?,
    ))
}

async fn remove_alias(
    State(state): State<AppState>,
    person: Person,
    Path((primary, alias)): Path<(SenatorName, SenatorName)>,
) -> Result<StatusCode, ApiFailure> {
    state
        .hub
        .roster_now()
        .may_act_as(&person.0, &primary)
        .map_err(|error| ApiFailure::forbidden(error.to_string()))?;
    state.hub.remove_alias(&person.0, &primary, &alias).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn me(State(state): State<AppState>, person: Person) -> Result<Json<Me>, ApiFailure> {
    // Every person can join every committee in the log.
    let committees = state.hub.committees().await?;
    let roster = state.hub.roster().await?;
    let office = roster.office();
    Ok(Json(Me {
        agents: roster.agents_of(&person.0).into_iter().cloned().collect(),
        senate: state.hub.senate_name().clone(),
        parent: state.hub.parent(),
        people: roster.people(),
        speaker: state.hub.desk_seat(),
        // Roots are judged where the built-in session works the desk.
        triaged: state.triaged || office.as_ref().is_some_and(|o| o.triages()),
        office,
        children: state.hub.children(),
        person: person.0,
        committees,
        epoch: state.hub.epoch().await?,
    }))
}

async fn append(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<NewEvent>, JsonRejection>,
) -> Result<Json<AppendResponse>, ApiFailure> {
    let committee = committee_path(path)?;
    let event = json_body(body)?;
    if event.committee != committee {
        return Err(ApiFailure::bad_request(format!(
            "body committee {} does not match path committee {committee}",
            event.committee
        )));
    }
    // Decoding keeps unknown address forms (`Address::Other`) so stored
    // events always read back; as input they are invalid.
    let unknown = std::iter::once(&event.from)
        .chain(&event.to)
        .chain(&event.on_behalf_of)
        .find(|a| matches!(a, Address::Other(_)));
    if let Some(unknown) = unknown {
        return Err(ApiFailure::bad_request(format!(
            "unknown address form \"{unknown}\""
        )));
    }
    // A request writes as a senator it may act as: the person, or one of
    // their own agents (seated now if this is its first contact); for the
    // Speaker's office, naming who acts; or for a child Senate's office,
    // as its registered Speaker. Not as the committee, a guest, or
    // somebody else.
    let mut event = event;
    let sender = match &event.from {
        Address::Senator(from) => {
            state.act_as(&person, from).await?;
            Sender::Senator
        }
        Address::Speaker => {
            state.preside(&person, &mut event)?;
            Sender::Speaker
        }
        Address::Child { senate, .. } => {
            roles::speak_for_child(&state, &person, senate)?;
            Sender::Child
        }
        Address::AnyAgent => {
            return Err(ApiFailure::bad_request(
                "from names one senator; senator::* is only for recipients",
            ));
        }
        other => {
            return Err(ApiFailure::forbidden(format!(
                "{} may only write as a senator it can act as, not as {other}",
                person.0
            )));
        }
    };
    roles::check_children_addressed(&state, &event)?;
    // A secretary answers on its principal's behalf: `from` itself (which
    // the caller may act as), `on_behalf_of` a senator it is registered as
    // a secretary of.
    let as_secretary = matches!(
        (&event.from, &event.on_behalf_of),
        (Address::Senator(from), Some(Address::Senator(principal)))
            if state.hub.roster_now().secretaries_of(principal).contains(from)
    );
    reject_server_provenance(&event, sender == Sender::Speaker || as_secretary)?;
    roles::reject_office_keys(&event, sender)?;
    let roster = state.hub.roster().await?;
    match sender {
        Sender::Speaker => check_speaker_write(&event)?,
        Sender::Child => {
            roles::check_child_write(&event)?;
            reject_speaker_keys(&event)?;
        }
        Sender::Senator => {
            reject_brief_top_and_agent_decisions(&roster, &event)?;
            reject_speaker_keys(&event)?;
        }
    }
    check_duty(&state, &roster, sender, &event).await?;
    check_checkpoint(&roster, &event)?;
    check_companion(&roster, &event)?;
    check_status(&state, &roster, &event).await?;
    let appended = roles::append_routed(&state, &roster, sender, event).await?;
    Ok(Json(AppendResponse {
        event: appended.event,
        duplicate: appended.duplicate,
    }))
}

/// Who an append is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sender {
    /// A senator the caller may act as.
    Senator,
    /// The Speaker's office.
    Speaker,
    /// A child Senate's Speaker's office, as its registered Speaker.
    Child,
}

/// Some facts only the server's own components may state: which group a
/// committee is bound to (the gateway rebuilds its table from `binding`
/// events), presence, what a human said on a chat platform (`chat` is
/// platform-originated by definition: the Speaker interprets it as a person's
/// words and bridges present it as coming from the platform), speaking for
/// someone else, and platform provenance (`source`, `mirror`). A person's
/// bridge states none of them; an agent holding its person's token could
/// otherwise forge a human's instructions.
///
/// The Speaker speaks for others (`on_behalf_of`): when a senator holds its
/// seat, its writes may carry it ([`check_speaker_write`] says for whom).
/// [`meta::PICK`] is the server's own: it is set when a message is put to
/// the Speaker, never by a writer.
fn reject_server_provenance(event: &NewEvent, as_speaker: bool) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::COMMITTEE_SEAT) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is set by the server for committee seat changes",
            meta::COMMITTEE_SEAT
        )));
    }
    if matches!(event.kind, Kind::Binding | Kind::Presence | Kind::Chat) {
        return Err(ApiFailure::forbidden(format!(
            "{} events are written by the server, not by senators",
            event.kind
        )));
    }
    if event.meta.contains_key(meta::PICK) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is set by the server when it puts a message to the Speaker",
            meta::PICK
        )));
    }
    if event.meta.contains_key(meta::CHAIR_DISCUSSION) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is set by the server for cross-committee chair discussions",
            meta::CHAIR_DISCUSSION
        )));
    }
    let claimed = [
        ("on_behalf_of", event.on_behalf_of.is_some() && !as_speaker),
        ("source", event.source.is_some()),
        ("mirror", event.mirror.is_some()),
    ];
    if let Some((field, _)) = claimed.iter().find(|(_, set)| *set) {
        return Err(ApiFailure::forbidden(format!(
            "{field} is set by the server's own components, not by senators"
        )));
    }
    Ok(())
}

/// The rules about decisions and the brief. The "top three" header of the
/// brief is the Speaker's note ([`meta::BRIEF_TOP`]); a senator carrying the
/// key at all is refused, whatever its value, so no event in the log looks
/// like one that a bridge wrote. And agents only *propose* decisions (a
/// `note` with [`meta::PROPOSAL`]); a person confirms them, so a `decision`
/// from an agent is refused while one from a person stands.
fn reject_brief_top_and_agent_decisions(
    roster: &Roster,
    event: &NewEvent,
) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::BRIEF_TOP) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is written by the Speaker, not by senators",
            meta::BRIEF_TOP
        )));
    }
    if event.kind == Kind::Decision && roster.is_agent_address(&event.from) {
        return Err(ApiFailure::forbidden(format!(
            "agents propose decisions (a note with meta.{} = \"{}\"); people decide",
            meta::PROPOSAL,
            senate_core::PROPOSAL_DECISION
        )));
    }
    // Only a person confirms a decision: a team or a service principal
    // proposes, like an agent.
    if event.kind == Kind::Decision
        && let Some(from) = event.from.as_senator()
        && roster
            .principal(from.name())
            .is_some_and(|p| !p.kind.is_person())
    {
        return Err(ApiFailure::forbidden(format!(
            "{} is not a person: only a person confirms a decision",
            event.from
        )));
    }
    Ok(())
}

/// Triage verdicts and the output level are the Speaker's
/// ([`senate_core::task`]). A bridge holds a root until a verdict arrives,
/// so a senator able to write one could release its own question unjudged.
/// So are work runs ([`senate_core::work`]) and provisional answers; a
/// senator writing one could fake a run's card or its PR link.
fn reject_speaker_keys(event: &NewEvent) -> Result<(), ApiFailure> {
    for key in SPEAKER_KEYS {
        if event.meta.contains_key(key) {
            return Err(ApiFailure::forbidden(format!(
                "meta.{key} is written by the Speaker, not by senators"
            )));
        }
    }
    Ok(())
}

/// What a senator holding the Speaker's seat may not write as `speaker`:
/// work runs (the built-in Speaker's hands alone) and decisions (people
/// decide). It speaks `on_behalf_of` a senator of this Senate (a listed
/// person, or an agent: the away agent it covers, which need not have
/// connected yet; the person whose message it relays) or a guest; and, in
/// an answer for a child Senate that is offline
/// ([`meta::CHILD_OFFLINE`]), of that child's desk.
fn check_speaker_write(event: &NewEvent) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::WORK) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is the built-in Speaker's: a senator presiding does no work runs",
            meta::WORK
        )));
    }
    if event.kind == Kind::Decision {
        return Err(ApiFailure::forbidden(
            "the Speaker does not decide; people decide",
        ));
    }
    match &event.on_behalf_of {
        None | Some(Address::Guest(_)) | Some(Address::Senator(_)) => Ok(()),
        // The office answering for a child Senate that is offline.
        Some(Address::Child { .. })
            if event.kind == Kind::Answer && event.meta.contains_key(meta::CHILD_OFFLINE) =>
        {
            Ok(())
        }
        Some(other) => Err(ApiFailure::bad_request(format!(
            "the Speaker speaks on behalf of a senator or a guest, not {other}"
        ))),
    }
}

/// A checkpoint is one agent's own handover note, so only an agent writes
/// one (the caller is already known to be allowed to act as it), and it
/// carries a sound [`CheckpointPayload`].
fn check_checkpoint(roster: &Roster, event: &NewEvent) -> Result<(), ApiFailure> {
    if event.kind != Kind::Checkpoint {
        return Ok(());
    }
    if !roster.is_agent_address(&event.from) {
        return Err(ApiFailure::forbidden(format!(
            "a checkpoint is written by an agent's bridge, not by {}",
            event.from
        )));
    }
    let payload: CheckpointPayload = event
        .meta
        .get(meta::CHECKPOINT)
        .cloned()
        .ok_or_else(|| {
            ApiFailure::bad_request(format!("a checkpoint needs meta.{}", meta::CHECKPOINT))
        })
        .and_then(|v| {
            serde_json::from_value(v).map_err(|e| {
                ApiFailure::bad_request(format!(
                    "meta.{} is not a checkpoint: {e}",
                    meta::CHECKPOINT
                ))
            })
        })?;
    payload.check().map_err(ApiFailure::bad_request)
}

/// A seat's duty ([`meta::DUTY`]) rides on a `note` from a senator: the
/// senator itself, the person who owns it, or the committee's chair. The
/// Speaker's office, a child Senate and anyone else are refused.
async fn check_duty(
    state: &AppState,
    roster: &Roster,
    sender: Sender,
    event: &NewEvent,
) -> Result<(), ApiFailure> {
    let Some(value) = event.meta.get(meta::DUTY) else {
        return Ok(());
    };
    let (Sender::Senator, Some(from)) = (sender, event.from.as_senator()) else {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is set by the senator, its owner or the committee's chair, not by {}",
            meta::DUTY,
            event.from
        )));
    };
    if event.kind != Kind::Note {
        return Err(ApiFailure::bad_request(format!(
            "meta.{} rides on a note, not a {}",
            meta::DUTY,
            event.kind
        )));
    }
    let payload = DutyPayload::from_value(value).ok_or_else(|| {
        ApiFailure::bad_request(format!(
            "meta.{} is {{\"senator\": <name>, \"duty\": <text>}}",
            meta::DUTY
        ))
    })?;
    payload.check().map_err(ApiFailure::bad_request)?;
    if from == &payload.senator || roster.owner_of(&payload.senator) == Some(from.name()) {
        return Ok(());
    }
    let chair = state
        .hub
        .chair(&event.committee)
        .await
        .map_err(ApiFailure::from)?
        .chair;
    if chair.as_ref() == Some(from) {
        return Ok(());
    }
    Err(ApiFailure::forbidden(format!(
        "{} may not set the duty of {}: only that senator, its owner or the committee's chair",
        event.from,
        Address::senator(&payload.senator)
    )))
}

/// [`meta::COMPANION`] labels an agent's answer as its companion session's;
/// on anything else it would mislabel.
fn check_companion(roster: &Roster, event: &NewEvent) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::COMPANION)
        && !(event.kind == Kind::Answer && roster.is_agent_address(&event.from))
    {
        return Err(ApiFailure::bad_request(format!(
            "meta.{} only marks an agent's answer",
            meta::COMPANION
        )));
    }
    Ok(())
}

/// A `status` is a step of a task thread: it must be in the thread of a root
/// `question` / `request` addressed to an agent, and carry a sound
/// [`StatusPayload`]. An agent reports only on threads addressed to that very
/// agent (another agent of the same person does not qualify), and never
/// takes a thread over: only people do.
async fn check_status(
    state: &AppState,
    roster: &Roster,
    event: &NewEvent,
) -> Result<(), ApiFailure> {
    if event.kind != Kind::Status {
        return Ok(());
    }
    let Some(thread) = event.thread else {
        return Err(ApiFailure::bad_request(
            "a status must name its task thread",
        ));
    };
    let payload: StatusPayload = event
        .meta
        .get(meta::STATUS)
        .cloned()
        .ok_or_else(|| ApiFailure::bad_request(format!("a status needs meta.{}", meta::STATUS)))
        .and_then(|v| {
            serde_json::from_value(v).map_err(|e| {
                ApiFailure::bad_request(format!("meta.{} is not a status: {e}", meta::STATUS))
            })
        })?;
    payload.check().map_err(ApiFailure::bad_request)?;
    let root = state.hub.get(&event.committee, thread).await?;
    if let Some(root) = root
        .as_ref()
        .filter(|r| r.is_hosted_approval(&roster.people()))
    {
        // Speaker writes have already passed AppState::preside.
        if (event.from == root.from || event.from == Address::Speaker)
            && payload == StatusPayload::new(StatusState::Archived)
            && event.meta.get(meta::HOSTED_APPROVAL) == Some(&serde_json::json!(root.id))
        {
            return Ok(());
        }
        return Err(ApiFailure::forbidden(
            "only the originating senator or the Speaker's office may archive this hosted approval notice",
        ));
    }
    let task = root.as_ref().filter(|r| {
        r.thread.is_none()
            && matches!(r.kind, Kind::Question | Kind::Request)
            && r.to.iter().any(|a| roster.is_agent_address(a))
    });
    let Some(root) = task else {
        return Err(ApiFailure::bad_request(format!(
            "seq {thread} is not the root of a task thread"
        )));
    };
    if roster.is_agent_address(&event.from) {
        if !root.to.contains(&event.from) {
            return Err(ApiFailure::forbidden(format!(
                "{} may only report on threads addressed to it",
                event.from
            )));
        }
        if payload.takeover.is_some() {
            return Err(ApiFailure::forbidden("only people take a thread over"));
        }
    }
    Ok(())
}

async fn events(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Json<EventsPage>, ApiFailure> {
    let committee = committee_path(path)?;
    let params = query(params)?;
    let limit = params.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let mut events = state.hub.read(&committee, params.after, limit + 1).await?;
    if events.is_empty() {
        // Only an empty page can be a cursor from another epoch.
        reject_cursor_ahead(&state, &committee, params.after).await?;
    }
    let has_more = events.len() > limit;
    events.truncate(limit);
    Ok(Json(EventsPage { events, has_more }))
}

/// 409 `cursor_ahead` when `after` is beyond the committee's head (a committee
/// with no events has head 0): the client's cursor belongs to another epoch
/// of the log.
async fn reject_cursor_ahead(
    state: &AppState,
    committee: &CommitteeId,
    after: Seq,
) -> Result<(), ApiFailure> {
    if after == Seq::ZERO {
        return Ok(());
    }
    let head = state.hub.head(committee).await?;
    if after > head {
        return Err(ApiFailure::cursor_ahead(after, head));
    }
    Ok(())
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<Seq>, ApiFailure> {
    let Some(value) = headers.get("last-event-id") else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| ApiFailure::bad_request("Last-Event-ID is not ASCII"))?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| ApiFailure::bad_request(format!("Last-Event-ID must be a seq: {text:?}")))
}

async fn stream(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<StreamQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    let committee = committee_path(path)?;
    let params = query(params)?;
    // The agent this stream is the connection of: its presence follows. A
    // stream naming a person, or nobody, only reads.
    let agent = match params.senator {
        Some(senator) => {
            state.connect_as(&person, &senator).await?;
            let roster = state.hub.roster_now();
            Some(senator).filter(|s| !roster.is_person(s))
        }
        None => None,
    };
    let after = last_event_id(&headers)?.unwrap_or(params.after);
    // Before attaching, so a refused stream never counts toward presence.
    reject_cursor_ahead(&state, &committee, after).await?;
    // Live first, so the backlog read cannot miss a commit.
    let live = state.hub.open_stream(&committee, agent.as_ref()).await?;
    let feed = sse::feed(state.hub.clone(), committee, live, after, state.ping_every);
    let mut response = Sse::new(feed).into_response();
    // Ask reverse proxies not to buffer the stream.
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

async fn get_cursor(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<(String, String)>, PathRejection>,
    params: Result<Query<CursorQuery>, QueryRejection>,
) -> Result<Json<Cursor>, ApiFailure> {
    let (committee, name) = cursor_path(path)?;
    let params = query(params)?;
    let owner = if params.peek {
        let owner = params.senator.unwrap_or(
            SenatorName::new(person.0.clone()).map_err(|e| ApiFailure::forbidden(e.to_string()))?,
        );
        state
            .hub
            .roster_now()
            .may_act_as(&person.0, &owner)
            .map_err(|e| ApiFailure::forbidden(e.to_string()))?;
        owner
    } else {
        let owner = state.act_as_or_self(&person, params.senator).await?;
        state.hub.take_seat(&committee, &owner).await;
        owner
    };
    let seq = state.hub.owned_cursor(&committee, &owner, &name).await?;
    Ok(Json(Cursor { seq }))
}

async fn put_cursor(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<(String, String)>, PathRejection>,
    params: Result<Query<CursorQuery>, QueryRejection>,
    body: Result<Json<Cursor>, JsonRejection>,
) -> Result<Json<Cursor>, ApiFailure> {
    let (committee, name) = cursor_path(path)?;
    let owner = state
        .act_as_or_self(&person, query(params)?.senator)
        .await?;
    let cursor = json_body(body)?;
    state.hub.take_seat(&committee, &owner).await;
    state
        .hub
        .set_owned_cursor(&committee, &owner, &name, cursor.seq)
        .await?;
    Ok(Json(cursor))
}

async fn heartbeat(
    State(state): State<AppState>,
    person: Person,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<Heartbeat>, JsonRejection>,
) -> Result<StatusCode, ApiFailure> {
    let committee = committee_path(path)?;
    let beat = json_body(body)?;
    if state.hub.roster_now().is_person(&beat.agent) {
        return Err(ApiFailure::bad_request(format!(
            "a heartbeat is an agent's, and {} is a person",
            beat.agent
        )));
    }
    state.connect_as(&person, &beat.agent).await?;
    state.hub.record_heartbeat(&committee, beat).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn search(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Json<SearchResults>, ApiFailure> {
    let committee = committee_path(path)?;
    let params = query(params)?;
    if params.q.trim().is_empty() {
        return Err(ApiFailure::bad_request("q must name at least one term"));
    }
    let limit = params.limit.unwrap_or(DEFAULT_SEARCH).clamp(1, MAX_SEARCH);
    let events = state.hub.search(&committee, &params.q, limit).await?;
    Ok(Json(SearchResults { events }))
}

async fn brief(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Brief>, ApiFailure> {
    let committee = committee_path(path)?;
    Ok(Json(state.hub.brief(&committee).await?))
}

async fn presence(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<PresencePage>, ApiFailure> {
    let committee = committee_path(path)?;
    Ok(Json(state.hub.presence(&committee).await?))
}

async fn latest_checkpoint(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<CheckpointQuery>, QueryRejection>,
) -> Result<Json<CheckpointRecord>, ApiFailure> {
    let committee = committee_path(path)?;
    let agent = state.hub.primary_name(&query(params)?.agent).await;
    state
        .hub
        .latest_checkpoint(&committee, &agent)
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiFailure::not_found(format!(
                "{} has no checkpoint on {committee}",
                Address::senator(&agent)
            ))
        })
}

async fn threads(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
    params: Result<Query<ThreadsQuery>, QueryRejection>,
) -> Result<Json<ThreadsPage>, ApiFailure> {
    let committee = committee_path(path)?;
    let params = query(params)?;
    let board = state.hub.tasks(&committee).await?;
    let threads = match &params.agent {
        Some(agent) => board.threads_for(&state.hub.primary_name(agent).await, params.state),
        None => board.threads(params.state),
    };
    Ok(Json(ThreadsPage {
        threads: threads.into_iter().cloned().collect(),
        head: board.head(),
    }))
}

/// `GET /v1/resolve`: whether a qualified name is in this Senate, answered
/// for this Senate alone ([`ServerHub::resolve`]).
async fn resolve(
    State(state): State<AppState>,
    _person: Person,
    params: Result<Query<ResolveQuery>, QueryRejection>,
) -> Result<Json<Resolved>, ApiFailure> {
    let params = query(params)?;
    let resolved = state
        .hub
        .resolve(&params.name, params.committee.as_ref())
        .await?;
    Ok(Json(resolved))
}

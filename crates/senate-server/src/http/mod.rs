//! The HTTP/SSE API, exactly as `senate_core::api` documents it.

mod error;
mod sse;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::map_response_with_state;
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use senate_core::api::{
    AppendResponse, CheckpointQuery, Cursor, CursorQuery, DEFAULT_PAGE, DEFAULT_SEARCH,
    EPOCH_HEADER, EventsPage, EventsQuery, Heartbeat, MAX_PAGE, MAX_SEARCH, Me, PresencePage,
    SearchQuery, SearchResults, StreamQuery, ThreadsPage, ThreadsQuery,
};
use senate_core::{
    Address, Brief, CheckpointPayload, CheckpointRecord, CommitteeId, DocketPage, Hub, Kind, Name,
    NewEvent, PickPayload, ResolveError, SenatorPath, Seq, StatusPayload, meta,
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
        .route("/v1/committees/{committee}/search", get(search))
        .route("/v1/committees/{committee}/brief", get(brief))
        .route("/v1/committees/{committee}/threads", get(threads))
        .route("/v1/committees/{committee}/presence", get(presence))
        .route(
            "/v1/committees/{committee}/checkpoints/latest",
            get(latest_checkpoint),
        )
        .route("/v1/committees/{committee}/docket", get(docket))
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
    /// first contact. Anyone else's is 403.
    fn act_as(&self, person: &Person, senator: &SenatorPath) -> Result<(), ApiFailure> {
        self.hub
            .act_as(&person.0, senator)
            .map(|_| ())
            .map_err(|e| ApiFailure::forbidden(e.to_string()))
    }

    /// `person` writes as `speaker`: only when a senator holds this Senate's
    /// Speaker's seat and it is one of `person`'s own agents. The built-in
    /// Speaker writes in process; nobody writes as it over HTTP.
    fn preside(&self, person: &Person) -> Result<(), ApiFailure> {
        match self.hub.speaker_seat() {
            Some(seat) if seat.held_by(&person.0) => Ok(()),
            Some(seat) => Err(ApiFailure::forbidden(format!(
                "{} does not hold the Speaker's seat here ({})",
                person.0,
                match seat.senator() {
                    Some(senator) => format!("held by {}", Address::senator(senator)),
                    None => "the built-in Speaker".to_owned(),
                }
            ))),
            None => Err(ApiFailure::forbidden(
                "nobody presides over this Senate; nobody writes as the Speaker",
            )),
        }
    }

    /// Like [`AppState::act_as`], for a connection rather than a write: a
    /// stream, a cursor, a heartbeat. Besides the senators `person` may act
    /// as, the senator holding this Senate's Speaker's seat, when it is
    /// `person`'s: the presiding session follows this Senate's log as its
    /// Speaker, with its own cursors and presence.
    ///
    /// Where the holder could also sit in this Senate (the top Senate of a
    /// machine used alone: the Speaker is one of the person's own agents,
    /// directly under them) its contact seats it like any agent's, so that
    /// `senator::<person>/*` counts it. Where it could not (a nested Senate,
    /// presided over by an agent that sits in the Senate above) it is let
    /// in without a seat.
    fn connect_as(&self, person: &Person, senator: &SenatorPath) -> Result<(), ApiFailure> {
        let presides = self
            .hub
            .speaker_seat()
            .is_some_and(|seat| seat.held_by(&person.0) && seat.senator() == Some(senator));
        match self.act_as(person, senator) {
            Err(_) if presides => Ok(()),
            result => result,
        }
    }

    /// Like [`AppState::connect_as`], for a request that may name no
    /// senator: it is then the person's own.
    fn act_as_or_self(
        &self,
        person: &Person,
        senator: Option<SenatorPath>,
    ) -> Result<SenatorPath, ApiFailure> {
        let senator = senator.unwrap_or_else(|| SenatorPath::senior(person.0.clone()));
        self.connect_as(person, &senator)?;
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

async fn me(State(state): State<AppState>, person: Person) -> Result<Json<Me>, ApiFailure> {
    // Every person can join every committee in the log.
    let committees = state.hub.committees().await?;
    let roster = state.hub.roster().await?;
    Ok(Json(Me {
        agents: roster.agents_of(&person.0).into_iter().cloned().collect(),
        presider: roster.presider().cloned(),
        speaker: state.hub.speaker_seat().cloned(),
        person: person.0,
        committees,
        epoch: state.hub.epoch().await?,
        triaged: state.triaged,
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
    // their own agents (seated now if this is its first contact); or as the
    // Speaker when one of their agents holds its seat. Not as the
    // committee, a guest, or somebody else.
    let as_speaker = match &event.from {
        Address::Senator(from) => {
            state.act_as(&person, from)?;
            false
        }
        Address::Speaker => {
            state.preside(&person)?;
            true
        }
        Address::AgentOf(_) => {
            return Err(ApiFailure::bad_request(
                "from names one senator; senator::<person>/* is only for recipients",
            ));
        }
        other => {
            return Err(ApiFailure::forbidden(format!(
                "{} may only write as a senator it can act as, not as {other}",
                person.0
            )));
        }
    };
    reject_server_provenance(&event, as_speaker)?;
    check_recipients(&state, &event).await?;
    if as_speaker {
        check_speaker_write(&state, &event).await?;
    } else {
        reject_brief_top_and_agent_decisions(&event)?;
        reject_speaker_keys(&event)?;
    }
    check_checkpoint(&event)?;
    check_companion(&event)?;
    check_status(&state, &event).await?;
    // `to` may hold `senator::<person>/*`: resolved against the roster, put
    // to the Speaker to pick, or refused with the candidates.
    let event = put_to_speaker(&state, event).await?;
    let appended = state.hub.resolve_and_append(event).await?;
    Ok(Json(AppendResponse {
        event: appended.event,
        duplicate: appended.duplicate,
    }))
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
fn reject_brief_top_and_agent_decisions(event: &NewEvent) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::BRIEF_TOP) {
        return Err(ApiFailure::forbidden(format!(
            "meta.{} is written by the Speaker, not by senators",
            meta::BRIEF_TOP
        )));
    }
    if event.kind == Kind::Decision && event.from.is_junior() {
        return Err(ApiFailure::forbidden(format!(
            "agents propose decisions (a note with meta.{} = \"{}\"); people decide",
            meta::PROPOSAL,
            senate_core::PROPOSAL_DECISION
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

/// Every senator an event is addressed to could sit in this Senate: a
/// listed person, or an agent at this Senate's level (seated or not yet).
/// An agent one level down, or another Senate's agent, is not reached from
/// here; a message for it would wait for nobody.
async fn check_recipients(state: &AppState, event: &NewEvent) -> Result<(), ApiFailure> {
    let roster = state.hub.roster().await?;
    for senator in event.to.iter().filter_map(Address::as_senator) {
        if let Err(e) = roster.may_act_as(senator.owner(), senator) {
            return Err(ApiFailure::bad_request(format!(
                "{} cannot be reached in this Senate: {e}",
                Address::senator(senator)
            )));
        }
    }
    Ok(())
}

/// What a senator holding the Speaker's seat may not write as `speaker`:
/// work runs (the built-in Speaker's hands alone) and decisions (people
/// decide). It speaks `on_behalf_of` a senator who could sit in this Senate
/// (a listed person, or an agent at this Senate's level: the away agent it
/// covers, which need not have connected yet; the person whose message it
/// relays) or a guest.
async fn check_speaker_write(state: &AppState, event: &NewEvent) -> Result<(), ApiFailure> {
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
        None | Some(Address::Guest(_)) => Ok(()),
        Some(Address::Senator(senator)) => {
            if state
                .hub
                .roster()
                .await?
                .may_act_as(senator.owner(), senator)
                .is_ok()
            {
                Ok(())
            } else {
                Err(ApiFailure::bad_request(format!(
                    "{} does not sit in this Senate",
                    Address::senator(senator)
                )))
            }
        }
        Some(other) => Err(ApiFailure::bad_request(format!(
            "the Speaker speaks on behalf of a senator or a guest, not {other}"
        ))),
    }
}

/// A sender that cannot be expected to know a person's agents (a person,
/// not an agent) wrote `senator::<owner>/*`, and `owner` has several agents
/// here: with a senator in the Speaker's seat, the message is put to the
/// Speaker instead (`to: speaker`, [`meta::PICK`] naming the candidates),
/// which routes it. An agent sender, or a Senate whose Speaker cannot pick
/// over HTTP (built in, or none), is refused with the candidates (409).
/// Several ambiguous wildcards in one message are refused too: one pick
/// per message.
async fn put_to_speaker(state: &AppState, mut event: NewEvent) -> Result<NewEvent, ApiFailure> {
    let senator_speaker = state
        .hub
        .speaker_seat()
        .is_some_and(|seat| seat.senator().is_some());
    if !senator_speaker || event.from.is_junior() || event.from == Address::Speaker {
        return Ok(event);
    }
    let roster = state.hub.roster().await?;
    let mut pick: Option<PickPayload> = None;
    let mut to = Vec::with_capacity(event.to.len());
    for address in &event.to {
        match roster.resolve_address(address) {
            Err(ResolveError::Ambiguous { owner, candidates }) if pick.is_none() => {
                pick = Some(PickPayload { owner, candidates });
                if !to.contains(&Address::Speaker) {
                    to.push(Address::Speaker);
                }
            }
            // Refused below, by the ordinary resolution.
            Err(_) => return Ok(event),
            Ok(_) => to.push(address.clone()),
        }
    }
    if let Some(pick) = pick {
        event.to = to;
        event.meta.insert(meta::PICK.into(), pick.value());
    }
    Ok(event)
}

/// A checkpoint is one agent's own handover note, so only an agent writes
/// one (the caller is already known to be allowed to act as it), and it
/// carries a sound [`CheckpointPayload`].
fn check_checkpoint(event: &NewEvent) -> Result<(), ApiFailure> {
    if event.kind != Kind::Checkpoint {
        return Ok(());
    }
    if !event.from.is_junior() {
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

/// [`meta::COMPANION`] labels an agent's answer as its companion session's;
/// on anything else it would mislabel.
fn check_companion(event: &NewEvent) -> Result<(), ApiFailure> {
    if event.meta.contains_key(meta::COMPANION)
        && !(event.kind == Kind::Answer && event.from.is_junior())
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
/// agent (full paths: a sibling agent of the same person does not qualify),
/// and never takes a thread over: only people do.
async fn check_status(state: &AppState, event: &NewEvent) -> Result<(), ApiFailure> {
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
    let task = root.as_ref().filter(|r| {
        r.thread.is_none()
            && matches!(r.kind, Kind::Question | Kind::Request)
            && r.to.iter().any(Address::is_junior)
    });
    let Some(root) = task else {
        return Err(ApiFailure::bad_request(format!(
            "seq {thread} is not the root of a task thread"
        )));
    };
    if event.from.is_junior() {
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
            state.connect_as(&person, &senator)?;
            Some(senator).filter(SenatorPath::is_junior)
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
    let owner = state.act_as_or_self(&person, query(params)?.senator)?;
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
    let owner = state.act_as_or_self(&person, query(params)?.senator)?;
    let cursor = json_body(body)?;
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
    if beat.agent.is_senior() {
        return Err(ApiFailure::bad_request(format!(
            "a heartbeat is an agent's, and {} is a person",
            beat.agent
        )));
    }
    state.connect_as(&person, &beat.agent)?;
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
    let agent = query(params)?.agent;
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

async fn docket(
    State(state): State<AppState>,
    _person: Person,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<DocketPage>, ApiFailure> {
    let committee = committee_path(path)?;
    Ok(Json(state.hub.docket(&committee).await?))
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
        Some(agent) => board.threads_for(agent, params.state),
        None => board.threads(params.state),
    };
    Ok(Json(ThreadsPage {
        threads: threads.into_iter().cloned().collect(),
        head: board.head(),
    }))
}

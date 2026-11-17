//! Test support: a fake Senate server (the subset of `senate_core::api`
//! the bridge uses), a fake Claude inbox socket, and a harness that runs the
//! daemon against both in a temp `SENATE_HOME`. Nothing here touches the
//! real home directory or any real Claude Code session.
//!
//! The fake server authenticates people, not agents: a token names a person,
//! and everything done as an agent names it in the query (`?senator=`,
//! `?agent=`) or in the body (`from`, `Heartbeat::agent`). It seats an agent
//! on first contact, owned by the person who first acts as it, and resolves
//! `senator::*` against the agents it has seated, refusing an ambiguous one
//! with the candidates, as `senate_core::api` says the real server does. It
//! answers `GET /v1/resolve` for itself: a Senate named `senate` (or
//! [`FakeServer::set_senate`]) with no Senate above.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
// Handlers return a ready `Response` as their error, as axum handlers do.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use senate_bridge::hook::{self, HookEnv, HookKind, HookTimeouts};
use senate_bridge::state::Store;
use senate_bridge::{BridgeConfig, DaemonHandle, DaemonOptions, Paths, Timing, daemon};
use senate_core::api::{
    ApiError, AppendResponse, CheckpointQuery, Cursor, CursorQuery, DEFAULT_SEARCH, EPOCH_HEADER,
    EventsQuery, Heartbeat, MAX_SEARCH, Me, ResolveQuery, Resolved, SearchQuery, SearchResults,
    StreamQuery, ThreadsQuery,
};
use senate_core::hub::{matches_terms, search_terms};
use senate_core::{
    Address, Brief, CommitteeId, Event, Kind, Locale, Locator, Name, NewEvent, Principal, Roster,
    SenatePath, SenatorName, Seq, Source,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

pub mod fake_codex;

// ---- addresses and events ---------------------------------------------------

pub fn committee() -> CommitteeId {
    CommitteeId::new("refund-v2").unwrap()
}

pub fn name(h: &str) -> Name {
    Name::new(h).unwrap()
}

pub fn path(p: &str) -> SenatorName {
    p.parse().unwrap()
}

/// `senator::<p>`: a person (`alice`) or an agent (`claude`).
pub fn senator(p: &str) -> Address {
    Address::senator(&path(p))
}

/// The Claude Code agent `person` runs: alice's (whose machine the harness
/// is) is `senator::claude`, the name her sessions speak as by default;
/// another person's is `senator::copilot`, since an agent's name is one
/// flat name and one person runs it.
pub fn claude(person: &str) -> Address {
    senator(if person == "alice" {
        "claude"
    } else {
        "copilot"
    })
}

/// The Codex agent `person` runs: alice's is `senator::codex`; another
/// person's is `senator::aider`.
pub fn codex(person: &str) -> Address {
    senator(if person == "alice" { "codex" } else { "aider" })
}

/// The person `senator::<person>`.
pub fn person(person: &str) -> Address {
    senator(person)
}

/// The Speaker relaying "tell alice's agent …" on behalf of senator::pm.
pub fn relayed_question(body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Question, Address::Speaker, body)
        .on_behalf_of(person("pm"))
        .to(claude("alice"))
}

/// The Speaker relaying a question from senator::pm to `to`.
pub fn relayed_to(to: Address, body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Question, Address::Speaker, body)
        .on_behalf_of(person("pm"))
        .to(to)
}

pub fn question(from: Address, to: Address, body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Question, from, body).to(to)
}

pub fn note(from: Address, body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Note, from, body)
}

/// Where a chat gateway says a message came from.
pub fn chat_source() -> Source {
    Source {
        platform: "chat".into(),
        group_id: "backend".into(),
        group_name: Some("退款 v2 · 后端群".into()),
    }
}

/// Poll `f` until it returns `Some`, or panic after `timeout`.
pub async fn eventually<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub const WAIT: Duration = Duration::from_secs(10);

// ---- fake server ------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct StreamOpen {
    pub committee: String,
    /// The authenticated person.
    pub person: String,
    /// The agent whose connection it is (`?senator=`).
    pub senator: Option<String>,
    pub last_event_id: Option<String>,
    pub after: u64,
    pub accepted: bool,
    /// Refused with 409 `cursor_ahead`.
    pub ahead: bool,
}

pub struct Inner {
    pub events: BTreeMap<String, Vec<Event>>,
    /// `(committee, seq)` → the JSON served in place of the stored event.
    pub raw: HashMap<(String, u64), String>,
    /// `(committee, whose, name)`: `whose` is the senator in the query, else
    /// the authenticated person.
    pub cursors: HashMap<(String, String, String), Seq>,
    /// `(committee, person, heartbeat)`.
    pub heartbeats: Vec<(String, String, Heartbeat)>,
    pub stream_opens: Vec<StreamOpen>,
    /// `(committee, senator or person)` → open streams.
    pub active: HashMap<(String, String), usize>,
    /// Who sits here; agents are seated on first contact.
    pub roster: Roster,
    /// The Senate's name `/v1/me` gives.
    pub senate: Name,
    /// The Senate above, as `/v1/me` gives it.
    pub parent: Option<SenatePath>,
    /// Who `/v1/me` says holds the Speaker's seat; unset: nobody.
    pub speaker: Option<senate_core::SpeakerSeat>,
    /// What `/v1/me` says about a Speaker judging roots here.
    pub triaged: bool,
    pub offline: bool,
    /// Sent as `x-senate-epoch` on every `/v1` response; empty sends none.
    pub epoch: String,
    /// Every `GET .../search`: `(committee, q, limit)`.
    pub searches: Vec<(String, String, Option<usize>)>,
    /// `GET .../brief` answers 500 while set.
    pub brief_broken: bool,
    /// `GET .../brief` answers only after this long.
    pub brief_delay: Duration,
    /// Every `GET /v1/resolve`: `(name, committee)`.
    pub resolves: Vec<(String, Option<String>)>,
}

impl Inner {
    /// This Senate's path.
    fn senate_path(&self) -> SenatePath {
        match &self.parent {
            Some(p) => p.clone().absolute().child(self.senate.clone()).unwrap(),
            None => SenatePath::top(self.senate.clone()).unwrap(),
        }
    }

    fn json_of(&self, committee: &str, e: &Event) -> String {
        self.raw
            .get(&(committee.to_string(), e.seq.0))
            .cloned()
            .unwrap_or_else(|| serde_json::to_string(e).unwrap())
    }

    fn head(&self, committee: &str) -> u64 {
        self.events.get(committee).map_or(0, |v| v.len() as u64)
    }
}

pub struct FakeState {
    pub inner: Mutex<Inner>,
    /// Live events with the JSON to send for each.
    live: broadcast::Sender<(Event, String)>,
    kill: watch::Sender<u64>,
    /// token → person.
    tokens: HashMap<String, String>,
    committees: Vec<CommitteeId>,
}

impl FakeState {
    /// Store `ev`; `edit` turns its JSON into what the server will serve.
    fn store_with(
        &self,
        ev: NewEvent,
        broadcast: bool,
        edit: Option<&dyn Fn(&mut Value)>,
    ) -> (Event, bool) {
        let mut inner = self.inner.lock().unwrap();
        let committee = ev.committee.to_string();
        let log = inner.events.entry(committee.clone()).or_default();
        if let Some(existing) = log.iter().find(|e| e.id == ev.id) {
            return (existing.clone(), true);
        }
        let seq = Seq(log.len() as u64 + 1);
        let event = ev.into_event(seq, Utc::now());
        log.push(event.clone());
        if let Some(edit) = edit {
            let mut v = serde_json::to_value(&event).unwrap();
            edit(&mut v);
            inner.raw.insert((committee.clone(), seq.0), v.to_string());
        }
        let data = inner.json_of(&committee, &event);
        drop(inner);
        if broadcast {
            // No receivers just means no stream is open.
            let _ = self.live.send((event.clone(), data));
        }
        (event, false)
    }

    fn store(&self, ev: NewEvent, broadcast: bool) -> (Event, bool) {
        self.store_with(ev, broadcast, None)
    }
}

pub struct FakeServer {
    pub url: String,
    pub state: Arc<FakeState>,
    task: JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(ApiError::new(code, message))).into_response()
}

/// May `person` act as `senator` here? Seats an agent on first contact,
/// owned by `person`.
fn act_as(st: &FakeState, person: &str, senator: &SenatorName) -> Result<(), Response> {
    let mut inner = st.inner.lock().unwrap();
    inner
        .roster
        .may_act_as(&name(person), senator)
        .map_err(|e| api_error(StatusCode::FORBIDDEN, "forbidden", &e.to_string()))?;
    if !inner.roster.is_person(senator) {
        let _ = inner.roster.seat(senator.clone(), &name(person));
    }
    Ok(())
}

fn cursor_ahead() -> Response {
    api_error(
        StatusCode::CONFLICT,
        "cursor_ahead",
        "after is beyond the committee's head",
    )
}

fn auth(st: &FakeState, headers: &HeaderMap) -> Result<String, Response> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|t| st.tokens.get(t).cloned())
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "unauthorized", "bad token"))
}

type St = State<Arc<FakeState>>;

async fn add_epoch(State(st): St, mut resp: Response) -> Response {
    let epoch = st.inner.lock().unwrap().epoch.clone();
    if !epoch.is_empty() {
        resp.headers_mut()
            .insert(EPOCH_HEADER, HeaderValue::from_str(&epoch).unwrap());
    }
    resp
}

async fn me(State(st): St, headers: HeaderMap) -> Response {
    match auth(&st, &headers) {
        Ok(h) => {
            let inner = st.inner.lock().unwrap();
            Json(Me {
                person: name(&h),
                senate: inner.senate.clone(),
                parent: inner.parent.clone(),
                speaker: inner.speaker.clone(),
                office: inner.roster.office(),
                children: Vec::new(),
                people: inner.roster.people(),
                agents: inner
                    .roster
                    .agents_of(&name(&h))
                    .into_iter()
                    .cloned()
                    .collect(),
                committees: st.committees.clone(),
                epoch: inner.epoch.clone(),
                triaged: inner.triaged,
            })
            .into_response()
        }
        Err(r) => r,
    }
}

async fn append(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    headers: HeaderMap,
    Json(ev): Json<NewEvent>,
) -> Response {
    let who = match auth(&st, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if ev.committee.as_str() != committee {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "committee mismatch");
    }
    let Some(from) = ev.from.as_senator().cloned() else {
        return api_error(StatusCode::FORBIDDEN, "forbidden", "not your address");
    };
    if let Err(r) = act_as(&st, &who, &from) {
        return r;
    }
    let mut ev = ev;
    let resolved = st.inner.lock().unwrap().roster.resolve_recipients(&ev.to);
    match resolved {
        Ok(to) => ev.to = to,
        Err(e) => {
            let refused = ApiError::from(&e);
            let status = if refused.code == "ambiguous_recipient" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            return (status, Json(refused)).into_response();
        }
    }
    let (event, duplicate) = st.store(ev, true);
    Json(AppendResponse { event, duplicate }).into_response()
}

async fn events(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    Query(q): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let limit = q.limit.unwrap_or(100).min(500);
    let inner = st.inner.lock().unwrap();
    if q.after.0 > inner.head(&committee) {
        return cursor_ahead();
    }
    let all: Vec<Value> = inner
        .events
        .get(&committee)
        .map(|v| {
            v.iter()
                .filter(|e| e.seq > q.after)
                .map(|e| serde_json::from_str(&inner.json_of(&committee, e)).unwrap())
                .collect()
        })
        .unwrap_or_default();
    let has_more = all.len() > limit;
    Json(json!({
        "events": all.into_iter().take(limit).collect::<Vec<_>>(),
        "has_more": has_more,
    }))
    .into_response()
}

/// Whose cursor a query names: the senator, else the person.
fn whose(st: &FakeState, person: &str, q: &CursorQuery) -> Result<String, Response> {
    match &q.senator {
        Some(s) => {
            act_as(st, person, s)?;
            Ok(s.to_string())
        }
        None => Ok(person.to_string()),
    }
}

async fn get_cursor(
    State(st): St,
    UrlPath((committee, name)): UrlPath<(String, String)>,
    Query(q): Query<CursorQuery>,
    headers: HeaderMap,
) -> Response {
    let who = match auth(&st, &headers).and_then(|p| whose(&st, &p, &q)) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let inner = st.inner.lock().unwrap();
    match inner.cursors.get(&(committee, who, name)) {
        Some(seq) => Json(Cursor { seq: *seq }).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "not_found", "no such cursor"),
    }
}

async fn put_cursor(
    State(st): St,
    UrlPath((committee, name)): UrlPath<(String, String)>,
    Query(q): Query<CursorQuery>,
    headers: HeaderMap,
    Json(c): Json<Cursor>,
) -> Response {
    let who = match auth(&st, &headers).and_then(|p| whose(&st, &p, &q)) {
        Ok(h) => h,
        Err(r) => return r,
    };
    st.inner
        .lock()
        .unwrap()
        .cursors
        .insert((committee, who, name), c.seq);
    Json(c).into_response()
}

async fn heartbeat(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    headers: HeaderMap,
    Json(hb): Json<Heartbeat>,
) -> Response {
    let who = match auth(&st, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if let Err(r) = act_as(&st, &who, &hb.agent) {
        return r;
    }
    st.inner
        .lock()
        .unwrap()
        .heartbeats
        .push((committee, who, hb));
    StatusCode::NO_CONTENT.into_response()
}

/// Every term in the body, newest first: the contract's exact matches,
/// without near matches.
async fn search(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    Query(q): Query<SearchQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let mut inner = st.inner.lock().unwrap();
    inner
        .searches
        .push((committee.clone(), q.q.clone(), q.limit));
    let terms = search_terms(&q.q);
    if terms.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "empty q");
    }
    let limit = q.limit.unwrap_or(DEFAULT_SEARCH).min(MAX_SEARCH);
    let events: Vec<Event> = inner
        .events
        .get(&committee)
        .map(|v| {
            v.iter()
                .rev()
                .filter(|e| matches_terms(&e.body, &terms))
                .take(limit)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Json(SearchResults { events }).into_response()
}

async fn brief(State(st): St, UrlPath(committee): UrlPath<String>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let delay = st.inner.lock().unwrap().brief_delay;
    tokio::time::sleep(delay).await;
    let inner = st.inner.lock().unwrap();
    if inner.brief_broken {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "brief broke");
    }
    let Ok(id) = CommitteeId::new(committee.as_str()) else {
        return api_error(StatusCode::BAD_REQUEST, "bad_request", "bad committee");
    };
    let events = inner.events.get(&committee).cloned().unwrap_or_default();
    Json(Brief::from_events(id, &events)).into_response()
}

async fn threads(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    Query(q): Query<ThreadsQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let inner = st.inner.lock().unwrap();
    let mut board = senate_core::TaskBoard::new(inner.roster.people());
    for e in inner.events.get(&committee).cloned().unwrap_or_default() {
        board.observe(&e);
    }
    let threads = match &q.agent {
        Some(agent) => board.threads_for(agent, q.state),
        None => board.threads(q.state),
    };
    Json(senate_core::api::ThreadsPage {
        threads: threads.into_iter().cloned().collect(),
        head: board.head(),
    })
    .into_response()
}

async fn latest_checkpoint(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    Query(q): Query<CheckpointQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let inner = st.inner.lock().unwrap();
    let mut all = senate_core::Checkpoints::new();
    for e in inner.events.get(&committee).cloned().unwrap_or_default() {
        all.observe(&e);
    }
    match all.latest(&q.agent) {
        Some(record) => Json(record.clone()).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "not_found", "no checkpoint"),
    }
}

/// `GET /v1/resolve`, as the real server answers it, for this one
/// Senate: a Senate path that is not this one's is elsewhere; a committee
/// it does not list is elsewhere unless the name says this Senate; a
/// senator nobody here knows is unknown.
async fn resolve(State(st): St, Query(q): Query<ResolveQuery>, headers: HeaderMap) -> Response {
    if let Err(r) = auth(&st, &headers) {
        return r;
    }
    let mut inner = st.inner.lock().unwrap();
    inner.resolves.push((
        q.name.to_string(),
        q.committee.as_ref().map(ToString::to_string),
    ));
    let path = inner.senate_path();
    let named_here = match q.name.senate() {
        None => false,
        Some(senate) if path.ends_with(senate) => true,
        Some(_) => return Json(Resolved::Elsewhere).into_response(),
    };
    let Some(target) = q.name.committee().or(q.committee.as_ref()).cloned() else {
        return Json(Resolved::Unknown {
            reason: format!("{} names no committee", q.name),
        })
        .into_response();
    };
    let exists = Some(&target) == q.committee.as_ref()
        || st.committees.contains(&target)
        || inner.events.contains_key(target.as_str());
    if !exists {
        return Json(if named_here {
            Resolved::Unknown {
                reason: format!("{path} has no committee {target}"),
            }
        } else {
            Resolved::Elsewhere
        })
        .into_response();
    }
    let address = q.name.address().clone();
    if let Some(senator) = address.as_senator()
        && !inner.roster.knows(senator)
    {
        return Json(Resolved::Unknown {
            reason: format!("{path}::{target} has no senator {senator}"),
        })
        .into_response();
    }
    Json(Resolved::Here {
        locator: Locator::at(path, target.clone(), address.clone()),
        committee: target,
        address,
    })
    .into_response()
}

fn frame(seq: Seq, data: &str) -> Bytes {
    Bytes::from(format!("event: event\nid: {seq}\ndata: {data}\n\n"))
}

async fn stream(
    State(st): St,
    UrlPath(committee): UrlPath<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    let who = match auth(&st, &headers) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if let Some(s) = &q.senator
        && let Err(r) = act_as(&st, &who, s)
    {
        return r;
    }
    let reader = q
        .senator
        .as_ref()
        .map_or_else(|| who.clone(), ToString::to_string);
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let after = last_event_id
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Seq)
        .unwrap_or(q.after);
    // Subscribe to live first, then read the backlog, then skip live events
    // already replayed: the server's no-gap rule.
    let mut live = st.live.subscribe();
    let mut kill = st.kill.subscribe();
    let backlog: Vec<(Seq, String)> = {
        let mut inner = st.inner.lock().unwrap();
        let ahead = after.0 > inner.head(&committee);
        let accepted = !inner.offline && !ahead;
        inner.stream_opens.push(StreamOpen {
            committee: committee.clone(),
            person: who,
            senator: q.senator.as_ref().map(ToString::to_string),
            last_event_id,
            after: after.0,
            accepted,
            ahead,
        });
        if inner.offline {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "internal", "offline");
        }
        if ahead {
            return cursor_ahead();
        }
        *inner
            .active
            .entry((committee.clone(), reader.clone()))
            .or_default() += 1;
        inner
            .events
            .get(&committee)
            .map(|v| {
                v.iter()
                    .filter(|e| e.seq > after)
                    .map(|e| (e.seq, inner.json_of(&committee, e)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(1024);
    let st2 = st.clone();
    tokio::spawn(async move {
        let mut last = after;
        let mut open = true;
        for (seq, data) in backlog {
            last = seq;
            if tx.send(Ok(frame(seq, &data))).await.is_err() {
                open = false;
                break;
            }
        }
        let mut ping = tokio::time::interval(Duration::from_millis(500));
        while open {
            open = tokio::select! {
                _ = kill.changed() => false,
                _ = tx.closed() => false,
                _ = ping.tick() => {
                    let p = Bytes::from(format!("event: ping\ndata: {}\n\n", Utc::now().to_rfc3339()));
                    tx.send(Ok(p)).await.is_ok()
                }
                r = live.recv() => match r {
                    Ok((e, data)) if e.committee.as_str() == committee && e.seq > last => {
                        last = e.seq;
                        tx.send(Ok(frame(e.seq, &data))).await.is_ok()
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => true,
                    Err(_) => false,
                },
            };
        }
        let mut inner = st2.inner.lock().unwrap();
        if let Some(n) = inner.active.get_mut(&(committee.clone(), reader)) {
            *n -= 1;
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap()
}

impl FakeServer {
    /// `people`: `(token, person)`. `committees`: what `/v1/me` lists. The
    /// people listed, plus `pm`, are the Senate's people; no agent is
    /// seated until it first connects.
    pub async fn start(people: &[(&str, &str)], committees: &[&str]) -> Self {
        let mut principals: Vec<Principal> = people
            .iter()
            .map(|(_, p)| Principal::new(name(p)))
            .collect();
        if !people.iter().any(|(_, p)| *p == "pm") {
            principals.push(Principal::new(name("pm")));
        }
        let roster = Roster::new(principals, Vec::new()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (live, _) = broadcast::channel(4096);
        let (kill, _) = watch::channel(0);
        let state = Arc::new(FakeState {
            inner: Mutex::new(Inner {
                events: BTreeMap::new(),
                raw: HashMap::new(),
                cursors: HashMap::new(),
                heartbeats: Vec::new(),
                stream_opens: Vec::new(),
                active: HashMap::new(),
                roster,
                senate: name("senate"),
                parent: None,
                speaker: None,
                triaged: true,
                offline: false,
                epoch: "epoch-1".into(),
                searches: Vec::new(),
                brief_broken: false,
                brief_delay: Duration::ZERO,
                resolves: Vec::new(),
            }),
            live,
            kill,
            tokens: people
                .iter()
                .map(|(t, h)| (t.to_string(), h.to_string()))
                .collect(),
            committees: committees
                .iter()
                .map(|f| CommitteeId::new(*f).unwrap())
                .collect(),
        });
        let app = Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/v1/me", get(me))
            .route(senate_core::api::RESOLVE_PATH, get(resolve))
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
            .route(
                "/v1/committees/{committee}/checkpoints/latest",
                get(latest_checkpoint),
            )
            .layer(axum::middleware::map_response_with_state(
                state.clone(),
                add_epoch,
            ))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}"),
            state,
            task,
        }
    }

    /// Append as the server would (any author), and fan out live.
    pub fn append(&self, ev: NewEvent) -> Event {
        self.state.store(ev, true).0
    }

    /// Store without fanning out: the live stream will skip it (a gap).
    pub fn append_silently(&self, ev: NewEvent) -> Event {
        self.state.store(ev, false).0
    }

    /// Append, but serve the event's JSON as `edit` leaves it (e.g. a form
    /// this bridge cannot decode). `live`: fan it out on open streams.
    pub fn append_raw(&self, ev: NewEvent, live: bool, edit: impl Fn(&mut Value)) -> Event {
        self.state.store_with(ev, live, Some(&edit)).0
    }

    /// Drop every open stream and refuse new ones (503) until back online.
    pub fn set_offline(&self, offline: bool) {
        self.state.inner.lock().unwrap().offline = offline;
        if offline {
            self.state.kill.send_modify(|v| *v += 1);
        }
    }

    /// The log starts over (a `memory:` restart, a restore): events and
    /// cursors are gone, open streams drop, and the epoch becomes `epoch`
    /// (`None` keeps it, as a server from before epochs would).
    pub fn reset_log(&self, epoch: Option<&str>) {
        {
            let mut inner = self.state.inner.lock().unwrap();
            inner.events.clear();
            inner.raw.clear();
            inner.cursors.clear();
            if let Some(e) = epoch {
                inner.epoch = e.to_string();
            }
        }
        self.state.kill.send_modify(|v| *v += 1);
    }

    pub fn events(&self, committee: &str) -> Vec<Event> {
        self.state
            .inner
            .lock()
            .unwrap()
            .events
            .get(committee)
            .cloned()
            .unwrap_or_default()
    }

    pub fn last_event(&self, committee: &str) -> Event {
        self.events(committee).pop().expect("no events")
    }

    pub fn cursor(&self, committee: &str, who: &str, name: &str) -> Option<Seq> {
        self.state
            .inner
            .lock()
            .unwrap()
            .cursors
            .get(&(committee.into(), who.into(), name.into()))
            .copied()
    }

    pub fn set_cursor(&self, committee: &str, who: &str, name: &str, seq: Seq) {
        self.state
            .inner
            .lock()
            .unwrap()
            .cursors
            .insert((committee.into(), who.into(), name.into()), seq);
    }

    pub fn searches(&self) -> Vec<(String, String, Option<usize>)> {
        self.state.inner.lock().unwrap().searches.clone()
    }

    /// Make `GET .../brief` fail (500) or work again.
    pub fn set_brief_broken(&self, broken: bool) {
        self.state.inner.lock().unwrap().brief_broken = broken;
    }

    /// Make `GET .../brief` answer only after `delay`.
    pub fn set_brief_delay(&self, delay: Duration) {
        self.state.inner.lock().unwrap().brief_delay = delay;
    }

    pub fn heartbeats(&self) -> Vec<(String, String, Heartbeat)> {
        self.state.inner.lock().unwrap().heartbeats.clone()
    }

    /// The heartbeats `agent` sent on `committee`.
    pub fn heartbeats_of(&self, committee: &str, agent: &str) -> Vec<Heartbeat> {
        self.heartbeats()
            .into_iter()
            .filter(|(c, _, hb)| c == committee && hb.agent.to_string() == agent)
            .map(|(_, _, hb)| hb)
            .collect()
    }

    pub fn stream_opens(&self) -> Vec<StreamOpen> {
        self.state.inner.lock().unwrap().stream_opens.clone()
    }

    /// Open streams on `committee`, of any reader.
    pub fn active_streams(&self, committee: &str) -> usize {
        self.state
            .inner
            .lock()
            .unwrap()
            .active
            .iter()
            .filter(|((c, _), _)| c == committee)
            .map(|(_, n)| *n)
            .sum()
    }

    /// Open streams on `committee` that are `senator`'s connection.
    pub fn active_streams_of(&self, committee: &str, senator: &str) -> usize {
        self.state
            .inner
            .lock()
            .unwrap()
            .active
            .get(&(committee.to_string(), senator.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Seat `agent`, run by `owner`, as if it had connected before.
    pub fn seat(&self, agent: &str, owner: &str) {
        self.state
            .inner
            .lock()
            .unwrap()
            .roster
            .seat(path(agent), &name(owner))
            .unwrap();
    }

    /// Make `/v1/me` name `speaker` as holding the Speaker's seat.
    pub fn set_speaker(&self, speaker: &str) {
        self.state.inner.lock().unwrap().speaker =
            Some(senate_core::SpeakerSeat::Senator(path(speaker)));
    }

    /// Make `/v1/me` name this Senate `senate`, under `parent`.
    pub fn set_senate(&self, senate: &str, parent: Option<&str>) {
        let mut inner = self.state.inner.lock().unwrap();
        inner.senate = name(senate);
        inner.parent = parent.map(|p| p.parse().unwrap());
    }

    /// Every `GET /v1/resolve` so far: `(name, committee)`.
    pub fn resolves(&self) -> Vec<(String, Option<String>)> {
        self.state.inner.lock().unwrap().resolves.clone()
    }
}

// ---- fake inbox -------------------------------------------------------------

/// One connection to a [`FakeInbox`].
#[derive(Debug, Clone)]
pub struct InboxConn {
    pub at: Instant,
    pub lines: Vec<String>,
}

/// A Unix socket that records every connection's lines, standing in for a
/// Claude Code session inbox. It answers nothing, unless given replies: the
/// first connections then each get one, written once the client has sent
/// everything.
pub struct FakeInbox {
    pub path: PathBuf,
    conns: Arc<Mutex<Vec<InboxConn>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeInbox {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeInbox {
    pub fn bind(path: PathBuf) -> Self {
        Self::replying(path, &[])
    }

    pub fn replying(path: PathBuf, replies: &[&str]) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let conns: Arc<Mutex<Vec<InboxConn>>> = Arc::default();
        let replies: Arc<Mutex<VecDeque<String>>> =
            Arc::new(Mutex::new(replies.iter().map(|r| r.to_string()).collect()));
        let c = conns.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let at = Instant::now();
                let reply = replies.lock().unwrap().pop_front();
                let c = c.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(r).lines();
                    let mut got = Vec::new();
                    while let Ok(Some(line)) = lines.next_line().await {
                        got.push(line);
                    }
                    if let Some(reply) = reply {
                        let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
                    }
                    c.lock().unwrap().push(InboxConn { at, lines: got });
                });
            }
        });
        Self { path, conns, task }
    }

    pub fn connections(&self) -> Vec<Vec<String>> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.lines.clone())
            .collect()
    }

    /// When each connection was accepted, in order.
    pub fn times(&self) -> Vec<Instant> {
        let mut t: Vec<Instant> = self.conns.lock().unwrap().iter().map(|c| c.at).collect();
        t.sort();
        t
    }

    /// The `content` of each connection's user frame, in arrival order.
    pub fn messages(&self) -> Vec<String> {
        let mut conns = self.conns.lock().unwrap().clone();
        conns.sort_by_key(|c| c.at);
        conns
            .iter()
            .filter_map(|c| {
                c.lines.iter().find_map(|l| {
                    let v: Value = serde_json::from_str(l).ok()?;
                    (v["type"] == "user")
                        .then(|| v["message"]["content"].as_str().unwrap().to_string())
                })
            })
            .collect()
    }

    pub async fn wait_messages(&self, n: usize) -> Vec<String> {
        eventually(&format!("{n} inbox message(s)"), WAIT, || {
            let m = self.messages();
            (m.len() >= n).then_some(m)
        })
        .await
    }
}

// ---- harness ----------------------------------------------------------------

pub fn fast_timing() -> Timing {
    Timing {
        heartbeat: Duration::from_millis(300),
        batch_window: Duration::from_millis(150),
        batch_max_wait: Duration::from_secs(2),
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(300),
        stall_timeout: Duration::from_secs(5),
        cursor_debounce: Duration::from_millis(50),
        sweep: Duration::from_millis(500),
        inbox_timeout: Duration::from_secs(1),
        push_spacing: Duration::from_millis(300),
        retry_base: Duration::from_millis(400),
        retry_max: Duration::from_secs(1),
        // No Speaker here: these tests model a bridge whose roots are never
        // judged, so nothing waits. `tests/tasks.rs` turns the wait on.
        triage_wait: Duration::ZERO,
        // Off unless a test turns it on (`tests/checkpoint.rs`).
        checkpoint_poll: Duration::from_secs(3600),
        checkpoint_interval: Duration::from_secs(7200),
    }
}

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
    pub server: FakeServer,
    pub config: BridgeConfig,
    pub daemon: Option<DaemonHandle>,
    /// A recording notifier for the daemon to post through; none by default,
    /// so a test never posts a real notification.
    pub desktop: Option<senate_bridge::notify::Desktop>,
}

impl Harness {
    /// alice's machine; the server knows alice and bob and two committees.
    pub async fn new() -> Self {
        Self::with_committees(&[]).await
    }

    /// `configured`: `committees = [...]` in bridge.toml.
    pub async fn with_committees(configured: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        // A push to a Codex session must never reach the user's real
        // app-server: point the daemon at a socket nobody listens on unless
        // a test binds a fake one there ([`Harness::codex`]).
        std::fs::create_dir_all(paths.home()).unwrap();
        std::fs::write(
            paths.config(),
            format!(
                "[codex]\nlisten = \"unix://{}\"\n",
                dir.path().join("codex.sock").display()
            ),
        )
        .unwrap();
        let server = FakeServer::start(
            &[("tok-alice", "alice"), ("tok-bob", "bob")],
            &["refund-v2", "other"],
        )
        .await;
        let mut config = BridgeConfig::new(&server.url, "tok-alice", name("alice"));
        config.committees = configured
            .iter()
            .map(|f| CommitteeId::new(*f).unwrap())
            .collect();
        // English unless a test sets `zh`, where it asserts Chinese.
        config.locale = Locale::En;
        Self {
            dir,
            paths,
            server,
            config,
            daemon: None,
            desktop: None,
        }
    }

    pub async fn start(&mut self) {
        self.start_with(fast_timing()).await;
    }

    pub async fn start_with(&mut self, timing: Timing) {
        let handle = daemon::start(DaemonOptions {
            paths: self.paths.clone(),
            config: self.config.clone(),
            timing,
            desktop: self.desktop.clone(),
        })
        .await
        .unwrap();
        self.daemon = Some(handle);
    }

    pub async fn stop(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.paths.socket()
    }

    pub fn store(&self) -> &Store {
        self.daemon.as_ref().expect("daemon not running").store()
    }

    /// claude's queue item for `seq` on refund-v2.
    pub fn item(&self, seq: u64) -> Option<senate_bridge::state::QueueItem> {
        self.item_of("claude", seq)
    }

    /// `agent`'s queue item for `seq` on refund-v2.
    pub fn item_of(&self, agent: &str, seq: u64) -> Option<senate_bridge::state::QueueItem> {
        self.store()
            .item(&path(agent), &committee(), Seq(seq))
            .unwrap()
    }

    /// A working directory, bound to `committee` through `.senate/committee`.
    pub fn workdir(&self, name: &str, committee: Option<&str>) -> PathBuf {
        let wd = self.dir.path().join("work").join(name);
        std::fs::create_dir_all(wd.join(".senate")).unwrap();
        if let Some(f) = committee {
            std::fs::write(wd.join(".senate/committee"), format!("{f}\n")).unwrap();
        }
        wd
    }

    pub fn inbox(&self, name: &str) -> FakeInbox {
        FakeInbox::bind(self.dir.path().join(format!("{name}.sock")))
    }

    /// The fake Codex app-server the daemon's `[codex] listen` names.
    pub fn codex(&self) -> fake_codex::FakeCodex {
        fake_codex::FakeCodex::bind(self.dir.path().join("codex.sock"))
    }

    /// Run a hook the way Codex would (`hook <event> --codex`).
    pub async fn hook_codex(&self, kind: HookKind, thread: &str, cwd: &Path) -> Option<Value> {
        self.hook_as(
            kind,
            thread,
            cwd,
            &HookEnv::default(),
            Some(senate_bridge::session::Runtime::Codex),
        )
        .await
    }

    pub fn env_for(&self, session_id: &str, inbox: Option<&Path>) -> HookEnv {
        HookEnv {
            socket: inbox.map(Path::to_path_buf),
            token: inbox.map(|_| format!("tok-{session_id}")),
            claude_pid: Some(std::process::id()),
            session_id: None,
            agent: None,
        }
    }

    /// Run a hook the way Claude Code would; the parsed output, if any.
    pub async fn hook(
        &self,
        kind: HookKind,
        session_id: &str,
        cwd: &Path,
        inbox: Option<&FakeInbox>,
    ) -> Option<Value> {
        let env = self.env_for(session_id, inbox.map(|i| i.path.as_path()));
        self.hook_with(kind, session_id, cwd, &env).await
    }

    /// [`Harness::hook`] with an explicit environment.
    pub async fn hook_with(
        &self,
        kind: HookKind,
        session_id: &str,
        cwd: &Path,
        env: &HookEnv,
    ) -> Option<Value> {
        self.hook_as(kind, session_id, cwd, env, None).await
    }

    /// [`Harness::hook_with`] for a given runtime (`None`: inferred).
    pub async fn hook_as(
        &self,
        kind: HookKind,
        session_id: &str,
        cwd: &Path,
        env: &HookEnv,
        runtime: Option<senate_bridge::session::Runtime>,
    ) -> Option<Value> {
        let stdin = serde_json::json!({
            "session_id": session_id,
            "cwd": cwd,
            "transcript_path": format!("/tmp/{session_id}.jsonl"),
            "hook_event_name": kind.event_name(),
            "source": "startup",
        })
        .to_string();
        hook::run_as(
            kind,
            &self.socket(),
            &stdin,
            env,
            runtime,
            HookTimeouts::default(),
        )
        .await
        .map(|line| serde_json::from_str(&line).unwrap())
    }

    pub async fn wait_stream(&self, committee: &str) {
        eventually(&format!("a stream on {committee}"), WAIT, || {
            (self.server.active_streams(committee) > 0).then_some(())
        })
        .await;
    }

    /// Wait for `agent`'s own stream on `committee`.
    pub async fn wait_stream_of(&self, committee: &str, agent: &str) {
        eventually(&format!("{agent}'s stream on {committee}"), WAIT, || {
            (self.server.active_streams_of(committee, agent) > 0).then_some(())
        })
        .await;
    }

    /// claude's delivered cursor on the server reaching `seq`.
    pub async fn wait_delivered_cursor(&self, committee: &str, seq: u64) {
        self.wait_delivered_cursor_of(committee, "claude", seq)
            .await;
    }

    pub async fn wait_delivered_cursor_of(&self, committee: &str, agent: &str, seq: u64) {
        eventually(
            &format!("{agent}'s delivered cursor {seq} on the server"),
            WAIT,
            || (self.server.cursor(committee, agent, "delivered") == Some(Seq(seq))).then_some(()),
        )
        .await;
    }
}

/// `additionalContext` of a hook's output.
pub fn context(output: &Value) -> &str {
    output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
}

/// Position of `seq <n>`'s header in a push.
pub fn pos(text: &str, seq: u64) -> Option<usize> {
    text.find(&format!("· seq {seq} ·"))
}

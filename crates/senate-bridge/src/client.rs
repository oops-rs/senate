//! HTTP client for the Senate server's `/v1` API ([`senate_core::api`]).
//! Outbound only: the laptop never listens on the network.
//!
//! Pages of events come back as [`RawPage`]: each event is still JSON, and
//! [`decode_event`] decodes them one at a time, so an event this build
//! cannot read costs only itself, never the page around it. Every response
//! names the log's epoch ([`api::EPOCH_HEADER`]); pages and errors carry it
//! so the daemon can tell a reset log from a gap.
//!
//! The token names a person. Whatever the bridge does as one of that
//! person's agents names the agent in the query (`?senator=` on streams and
//! cursors, `?agent=` on checkpoints and threads), so two agents of one
//! person never share a stream, a cursor or a checkpoint.

use std::time::Duration;

use senate_core::api::{
    self, AppendResponse, CheckpointQuery, Cursor, CursorQuery, Heartbeat, Me, PresencePage,
    SearchQuery, ThreadsPage, ThreadsQuery,
};
use senate_core::{Brief, CheckpointRecord, CommitteeId, Event, NewEvent, SenatorName, Seq};

use reqwest::header::HeaderMap;
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::Value;

/// [`api::ApiError::code`] of a stream or page request whose `after` is
/// beyond the committee's head.
pub const CURSOR_AHEAD: &str = api::code::CURSOR_AHEAD;

#[derive(Debug, thiserror::Error)]
pub enum ApiClientError {
    #[error("could not reach the Senate server: {0}")]
    Http(#[from] reqwest::Error),
    #[error("the Senate server answered {status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
        /// The log epoch the error response named, if any.
        epoch: Option<String>,
        /// With `ambiguous_recipient`: the agents to choose from.
        candidates: Vec<SenatorName>,
    },
}

impl ApiClientError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Api { status, .. } => Some(*status),
            Self::Http(e) => e.status().map(|s| s.as_u16()),
        }
    }

    /// 409 `cursor_ahead`: the cursor sent is beyond the committee's log.
    pub fn is_cursor_ahead(&self) -> bool {
        matches!(self, Self::Api { status: 409, code, .. } if code == CURSOR_AHEAD)
    }

    pub fn epoch(&self) -> Option<&str> {
        match self {
            Self::Api { epoch, .. } => epoch.as_deref(),
            Self::Http(_) => None,
        }
    }

    /// The server's machine code, if it answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            Self::Http(_) => None,
        }
    }

    /// 409 `ambiguous_recipient`: `senator::*` could reach several agents
    /// on the committee. The agents to choose from.
    pub fn ambiguous_candidates(&self) -> Option<&[SenatorName]> {
        match self {
            Self::Api {
                code, candidates, ..
            } if code == api::code::AMBIGUOUS_RECIPIENT => Some(candidates),
            _ => None,
        }
    }
}

/// The log epoch a response names; `None` when absent or empty (a server
/// from before epochs).
pub fn epoch_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(api::EPOCH_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `GET .../events` with every event left as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct RawPage {
    pub events: Vec<Value>,
    /// True when more events exist after the last one returned.
    pub has_more: bool,
    pub epoch: Option<String>,
}

/// One event of a [`RawPage`] or an SSE message, decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    Event(Box<Event>),
    /// Not an [`Event`] this build can read. `seq` is whatever the JSON says
    /// it is, if it says.
    Undecodable {
        seq: Option<Seq>,
        error: String,
    },
}

impl Decoded {
    pub fn seq(&self) -> Option<Seq> {
        match self {
            Self::Event(e) => Some(e.seq),
            Self::Undecodable { seq, .. } => *seq,
        }
    }
}

/// The `seq` an event's JSON claims, decodable or not.
pub fn seq_of(value: &Value) -> Option<Seq> {
    value.get("seq").and_then(Value::as_u64).map(Seq)
}

pub fn decode_event(value: Value) -> Decoded {
    let seq = seq_of(&value);
    match serde_json::from_value::<Event>(value) {
        Ok(e) => Decoded::Event(Box::new(e)),
        Err(e) => Decoded::Undecodable {
            seq,
            error: e.to_string(),
        },
    }
}

#[derive(Clone)]
pub struct ApiClient {
    base: String,
    token: String,
    http: Client,
    /// No overall timeout: SSE responses never finish. Stalls are caught by
    /// the stream loop's keepalive deadline instead.
    streaming: Client,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    pub fn new(server_url: &str, token: &str) -> Result<Self, ApiClientError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        let streaming = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            base: server_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http,
            streaming,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn authed(&self, req: RequestBuilder) -> RequestBuilder {
        req.bearer_auth(&self.token)
    }

    pub async fn set_alias(
        &self,
        primary: &SenatorName,
        alias: &SenatorName,
        initial: bool,
    ) -> Result<SenatorName, ApiClientError> {
        let request = senate_core::api::SetAlias {
            alias: alias.clone(),
            initial,
        };
        let response = self
            .authed(self.http.put(self.url(&format!("/v1/aliases/{primary}"))))
            .json(&request)
            .send()
            .await?;
        Ok(checked(response).await?.json().await?)
    }

    pub async fn aliases(&self) -> Result<senate_core::api::SenatorAliases, ApiClientError> {
        let response = self
            .authed(self.http.get(self.url("/v1/aliases")))
            .send()
            .await?;
        Ok(checked(response).await?.json().await?)
    }

    pub async fn me(&self) -> Result<Me, ApiClientError> {
        let resp = self
            .authed(self.http.get(self.url(api::ME_PATH)))
            .send()
            .await?;
        let resp = checked(resp).await?;
        let header = epoch_of(resp.headers());
        let mut me: Me = resp.json().await?;
        if me.epoch.is_empty()
            && let Some(epoch) = header
        {
            me.epoch = epoch;
        }
        Ok(me)
    }

    pub async fn append(&self, event: &NewEvent) -> Result<AppendResponse, ApiClientError> {
        let url = self.url(&api::events_path(&event.committee));
        let resp = self.authed(self.http.post(url)).json(event).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// Stored events with `seq > after`, ascending, each still JSON.
    pub async fn events(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: Option<usize>,
    ) -> Result<RawPage, ApiClientError> {
        #[derive(Deserialize)]
        struct Wire {
            events: Vec<Value>,
            #[serde(default)]
            has_more: bool,
        }
        let mut url = format!("{}?after={}", self.url(&api::events_path(committee)), after);
        if let Some(limit) = limit {
            url.push_str(&format!("&limit={limit}"));
        }
        let resp = checked(self.authed(self.http.get(url)).send().await?).await?;
        let epoch = epoch_of(resp.headers());
        let wire: Wire = resp.json().await?;
        Ok(RawPage {
            events: wire.events,
            has_more: wire.has_more,
            epoch,
        })
    }

    /// `senator`'s cursor `name` (the person's own when `None`); one never
    /// set reads as [`Seq::ZERO`].
    pub async fn cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        senator: Option<&SenatorName>,
    ) -> Result<Seq, ApiClientError> {
        let url = self.url(&api::cursor_path(committee, name));
        let query = CursorQuery {
            senator: senator.cloned(),
        };
        let resp = self.authed(self.http.get(url)).query(&query).send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(Seq::ZERO);
        }
        let cursor: Cursor = checked(resp).await?.json().await?;
        Ok(cursor.seq)
    }

    pub async fn put_cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        senator: Option<&SenatorName>,
        seq: Seq,
    ) -> Result<Seq, ApiClientError> {
        let url = self.url(&api::cursor_path(committee, name));
        let query = CursorQuery {
            senator: senator.cloned(),
        };
        let resp = self
            .authed(self.http.put(url))
            .query(&query)
            .json(&Cursor { seq })
            .send()
            .await?;
        let cursor: Cursor = checked(resp).await?.json().await?;
        Ok(cursor.seq)
    }

    pub async fn heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: &Heartbeat,
    ) -> Result<(), ApiClientError> {
        let url = self.url(&api::heartbeat_path(committee));
        let resp = self
            .authed(self.http.post(url))
            .json(heartbeat)
            .send()
            .await?;
        checked(resp).await?;
        Ok(())
    }

    /// `GET .../search`: matches for `q`, best first, each still JSON (see
    /// [`decode_event`]). The server clamps `limit` to [`api::MAX_SEARCH`].
    /// Search results have no next page, so `has_more` is always false.
    pub async fn search(
        &self,
        committee: &CommitteeId,
        q: &str,
        limit: Option<usize>,
    ) -> Result<RawPage, ApiClientError> {
        #[derive(Deserialize)]
        struct Wire {
            events: Vec<Value>,
        }
        let url = self.url(&api::search_path(committee));
        let query = SearchQuery {
            q: q.to_string(),
            limit,
        };
        let resp = self.authed(self.http.get(url)).query(&query).send().await?;
        let resp = checked(resp).await?;
        let epoch = epoch_of(resp.headers());
        let wire: Wire = resp.json().await?;
        Ok(RawPage {
            events: wire.events,
            has_more: false,
            epoch,
        })
    }

    /// `GET .../threads`: every task thread of the committee, or with
    /// `agent` only those addressed to that agent.
    pub async fn threads(
        &self,
        committee: &CommitteeId,
        agent: Option<&SenatorName>,
    ) -> Result<ThreadsPage, ApiClientError> {
        let url = self.url(&api::threads_path(committee));
        let query = ThreadsQuery {
            state: None,
            agent: agent.cloned(),
        };
        let resp = self.authed(self.http.get(url)).query(&query).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET .../brief`: the committee's brief, derived from its whole log.
    pub async fn brief(&self, committee: &CommitteeId) -> Result<Brief, ApiClientError> {
        let url = self.url(&api::brief_path(committee));
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET .../checkpoints/latest?agent=`: that agent's latest checkpoint;
    /// `None` when it has none (404). Another agent of the same person does
    /// not stand in for it.
    pub async fn latest_checkpoint(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<CheckpointRecord>, ApiClientError> {
        let url = self.url(&api::latest_checkpoint_path(committee));
        let query = CheckpointQuery {
            agent: agent.clone(),
        };
        let resp = self.authed(self.http.get(url)).query(&query).send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(checked(resp).await?.json().await?))
    }

    /// `GET .../docket`: what awaits the Speaker on the committee.
    pub async fn docket(
        &self,
        committee: &CommitteeId,
    ) -> Result<senate_core::DocketPage, ApiClientError> {
        let url = self.url(&api::docket_path(committee));
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET .../docket?role=chair`: what awaits the committee's chair.
    pub async fn chair_docket(
        &self,
        committee: &CommitteeId,
    ) -> Result<senate_core::DocketPage, ApiClientError> {
        let url = self.url(&api::docket_path(committee));
        let resp = self
            .authed(self.http.get(url))
            .query(&[("role", "chair")])
            .send()
            .await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET .../chair`: the committee's chair and how it was found.
    pub async fn chair(&self, committee: &CommitteeId) -> Result<api::ChairInfo, ApiClientError> {
        let url = self.url(&api::chair_path(committee));
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `PUT .../chair`: make `senator` the committee's chair.
    pub async fn set_chair(
        &self,
        committee: &CommitteeId,
        senator: &SenatorName,
    ) -> Result<api::ChairInfo, ApiClientError> {
        let url = self.url(&api::chair_path(committee));
        let body = api::SetChair {
            senator: senator.clone(),
        };
        let resp = self.authed(self.http.put(url)).json(&body).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    pub async fn set_committee_seat(
        &self,
        committee: &CommitteeId,
        request: &api::SetCommitteeSeat,
    ) -> Result<api::CommitteeSeatInfo, ApiClientError> {
        let url = self.url(&api::committee_seat_path(committee));
        let resp = self.authed(self.http.put(url)).json(request).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET /v1/secretaries`: every secretary relation.
    pub async fn secretaries(&self) -> Result<api::Secretaries, ApiClientError> {
        let url = self.url(api::SECRETARIES_PATH);
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `PUT /v1/secretaries/{senator}`: `senator`'s secretaries, in order;
    /// `acted_by` when writing for the Speaker's office.
    pub async fn set_secretaries(
        &self,
        senator: &SenatorName,
        secretaries: Vec<SenatorName>,
        acted_by: Option<SenatorName>,
    ) -> Result<api::Secretaries, ApiClientError> {
        let url = self.url(&api::secretaries_of_path(senator));
        let body = api::SetSecretaries {
            secretaries,
            acted_by,
        };
        let resp = self.authed(self.http.put(url)).json(&body).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    pub async fn set_secretary_session(
        &self,
        principal: &SenatorName,
        request: &senate_core::secretary_work::SetSecretarySession,
    ) -> Result<api::Secretaries, ApiClientError> {
        let url = self.url(&api::secretary_session_path(principal));
        let resp = self.authed(self.http.put(url)).json(request).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    pub async fn secretary_work(
        &self,
        principal: &SenatorName,
        request: &senate_core::secretary_work::SecretaryWorkRequest,
    ) -> Result<senate_core::secretary_work::SecretaryWorkReply, ApiClientError> {
        let url = self.url(&api::secretary_work_path(principal));
        let resp = self
            .authed(self.http.post(url))
            .json(request)
            .send()
            .await?;
        Ok(checked(resp).await?.json().await?)
    }

    pub async fn secretary_reminders(
        &self,
        senators: &[SenatorName],
    ) -> Result<Vec<senate_core::secretary_work::SecretaryReminder>, ApiClientError> {
        let url = self.url(api::SECRETARY_REMINDERS_PATH);
        let resp = self
            .authed(self.http.post(url))
            .json(senators)
            .send()
            .await?;
        Ok(checked(resp).await?.json().await?)
    }

    pub async fn ack_secretary_reminder(
        &self,
        reminder: &senate_core::secretary_work::SecretaryReminder,
    ) -> Result<(), ApiClientError> {
        let url = self.url(api::SECRETARY_REMINDERS_PATH);
        let resp = self
            .authed(self.http.put(url))
            .json(reminder)
            .send()
            .await?;
        checked(resp).await?;
        Ok(())
    }

    /// `PUT /v1/children/{name}`: register the child Senate `name`, its
    /// Speaker `speaker` (who this token must name).
    pub async fn register_child(
        &self,
        name: &senate_core::Name,
        speaker: &senate_core::SenatorId,
    ) -> Result<api::ChildSenate, ApiClientError> {
        let url = self.url(&api::child_path(name));
        let body = api::RegisterChild {
            speaker: speaker.clone(),
        };
        let resp = self.authed(self.http.put(url)).json(&body).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET /v1/children`: the child Senates registered here.
    pub async fn children(&self) -> Result<Vec<api::ChildSenate>, ApiClientError> {
        let url = self.url(api::CHILDREN_PATH);
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET /v1/resolve`: whether `name` is in this Senate, written from
    /// `committee` (the writer's own, when the writer stands here).
    pub async fn resolve(
        &self,
        name: &senate_core::Locator,
        committee: Option<&CommitteeId>,
    ) -> Result<api::Resolved, ApiClientError> {
        let query = api::ResolveQuery {
            name: name.clone(),
            committee: committee.cloned(),
        };
        let resp = self
            .authed(self.http.get(self.url(api::RESOLVE_PATH)))
            .query(&query)
            .send()
            .await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// `GET .../presence`: each agent's presence on the committee.
    pub async fn presence(&self, committee: &CommitteeId) -> Result<PresencePage, ApiClientError> {
        let url = self.url(&api::presence_path(committee));
        let resp = self.authed(self.http.get(url)).send().await?;
        Ok(checked(resp).await?.json().await?)
    }

    /// Open the committee's SSE stream after `after` as `agent`'s
    /// connection (it counts for that agent's presence). `Last-Event-ID`
    /// carries the same cursor (the server prefers the header). The log
    /// epoch is in the response's headers ([`epoch_of`]).
    pub async fn open_stream(
        &self,
        committee: &CommitteeId,
        after: Seq,
        agent: &SenatorName,
    ) -> Result<Response, ApiClientError> {
        let url = self.url(&api::stream_path(committee));
        let query = api::StreamQuery {
            after,
            senator: Some(agent.clone()),
        };
        let resp = self
            .authed(self.streaming.get(url))
            .query(&query)
            .header("Accept", "text/event-stream")
            .header("Last-Event-ID", after.to_string())
            .send()
            .await?;
        checked(resp).await
    }
}

async fn checked(resp: Response) -> Result<Response, ApiClientError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let epoch = epoch_of(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    Err(match serde_json::from_str::<api::ApiError>(&text) {
        Ok(e) => ApiClientError::Api {
            status,
            code: e.code,
            message: e.message,
            epoch,
            candidates: e.candidates,
        },
        Err(_) => ApiClientError::Api {
            status,
            code: "http".into(),
            message: text.chars().take(200).collect(),
            epoch,
            candidates: Vec::new(),
        },
    })
}

/// Resolve a qualified name nearest first: ask each Senate in `levels`
/// (the writer's own first, then the one above, outward) until one answers
/// for it. The writer stands on `committee` in the first Senate only. The
/// answer is the index of the Senate that answered and what it said;
/// [`Resolved::Elsewhere`](api::Resolved::Elsewhere) with the index past
/// the last when none did. There is no central registry: each Senate
/// answers for itself.
pub async fn resolve_outward(
    levels: &[&ApiClient],
    name: &senate_core::Locator,
    committee: Option<&CommitteeId>,
) -> Result<(usize, api::Resolved), ApiClientError> {
    for (i, level) in levels.iter().enumerate() {
        let standing = if i == 0 { committee } else { None };
        match level.resolve(name, standing).await? {
            api::Resolved::Elsewhere => continue,
            found => return Ok((i, found)),
        }
    }
    Ok((levels.len(), api::Resolved::Elsewhere))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decoding_is_per_event_and_keeps_the_seq() {
        let good = json!({
            "seq": 3, "committee": "refund-v2", "id": "01JB0000000000000000000000",
            "ts": "2026-10-22T01:14:03Z", "kind": "note", "from": "speaker", "body": "b"
        });
        assert!(matches!(decode_event(good), Decoded::Event(e) if e.seq == Seq(3)));
        let bad = json!({"seq": 4, "committee": "refund-v2", "id": "not-a-ulid"});
        let d = decode_event(bad);
        assert_eq!(d.seq(), Some(Seq(4)));
        assert!(matches!(d, Decoded::Undecodable { .. }));
        assert_eq!(decode_event(json!("junk")).seq(), None);
    }

    #[test]
    fn cursor_ahead_is_recognised() {
        let e = ApiClientError::Api {
            status: 409,
            code: CURSOR_AHEAD.into(),
            message: "x".into(),
            epoch: Some("e2".into()),
            candidates: Vec::new(),
        };
        assert!(e.is_cursor_ahead());
        assert_eq!(e.epoch(), Some("e2"));
        assert_eq!(e.ambiguous_candidates(), None);
        let other = ApiClientError::Api {
            status: 409,
            code: "conflict".into(),
            message: "x".into(),
            epoch: None,
            candidates: Vec::new(),
        };
        assert!(!other.is_cursor_ahead());
    }

    #[test]
    fn an_ambiguous_recipient_carries_its_candidates() {
        let candidates: Vec<SenatorName> =
            vec!["claude".parse().unwrap(), "codex".parse().unwrap()];
        let e = ApiClientError::Api {
            status: 409,
            code: api::code::AMBIGUOUS_RECIPIENT.into(),
            message: "bob has several agents here".into(),
            epoch: None,
            candidates: candidates.clone(),
        };
        assert_eq!(e.ambiguous_candidates(), Some(&candidates[..]));
        assert!(!e.is_cursor_ahead());
    }
}

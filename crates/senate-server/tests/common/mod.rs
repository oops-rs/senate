//! A server on 127.0.0.1:0 over a [`MemoryStore`], and a client for it.

#![allow(dead_code, clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use reqwest::{Method, RequestBuilder, StatusCode};
use senate_core::api::{ApiError, AppendResponse, Heartbeat};
use senate_core::{
    Address, CommitteeId, Event, Kind, Name, NewEvent, RosterConfig, SeatedAgent, SenatorName,
    Senior, Seq,
};
use senate_log::{LogStore, MemoryStore};
use senate_server::{Running, Server, ServerConfig, TokenConfig};

pub const ALICE: &str = "alice-dev-token";
pub const BOB: &str = "bob-dev-token";
pub const CAROL: &str = "carol-dev-token";
pub const WAIT: Duration = Duration::from_secs(5);

pub fn name(h: &str) -> Name {
    Name::new(h).unwrap()
}

pub fn path(p: &str) -> SenatorName {
    p.parse().unwrap()
}

/// `senator::<name>`: a person, or any senator named by its name.
pub fn person_addr(person: &str) -> Address {
    Address::Senator(path(person))
}

/// `senator::<agent>`: an agent, whose person is whoever first acts as it.
pub fn agent_addr(agent: &str) -> Address {
    Address::Senator(path(agent))
}

/// `agent`, seated in the roster under `owner` from the start.
pub fn seated(agent: &str, owner: &str) -> SeatedAgent {
    SeatedAgent::new(path(agent), name(owner))
}

pub fn committee() -> CommitteeId {
    CommitteeId::new("refund-v2").unwrap()
}

/// alice, bob and carol, with no agents seated: they are seated as they
/// connect.
pub fn roster() -> RosterConfig {
    RosterConfig {
        speaker: None,
        seniors: ["alice", "bob", "carol"]
            .into_iter()
            .map(|h| Senior::new(name(h)))
            .collect(),
        agents: Vec::new(),
    }
}

pub fn tokens() -> Vec<TokenConfig> {
    vec![
        TokenConfig::with_token(name("alice"), ALICE),
        TokenConfig::with_token(name("bob"), BOB),
        TokenConfig::with_token(name("carol"), CAROL),
    ]
}

pub struct TestServer {
    pub running: Running,
    pub client: reqwest::Client,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(Arc::new(MemoryStore::new()), None).await
    }

    pub async fn start_with(store: Arc<dyn LogStore>, ping_secs: Option<u64>) -> Self {
        Self::start_configured(store, |config| {
            if let Some(secs) = ping_secs {
                config.sse_ping_secs = secs;
            }
        })
        .await
    }

    pub async fn start_configured(
        store: Arc<dyn LogStore>,
        configure: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let mut config = ServerConfig::new(roster(), tokens());
        configure(&mut config);
        let running = Server::start(config, store).await.unwrap();
        Self {
            running,
            client: reqwest::Client::new(),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.running.base_url())
    }

    pub fn request(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        self.client
            .request(method, self.url(path))
            .bearer_auth(token)
    }

    pub async fn post_event(&self, token: &str, event: &NewEvent) -> reqwest::Response {
        self.request(
            Method::POST,
            &senate_core::api::events_path(&event.committee),
            token,
        )
        .json(event)
        .send()
        .await
        .unwrap()
    }

    /// Heartbeat as `agent` on the committee with `token`'s person: it is
    /// seated (under that person, when nobody answers for it yet) and is
    /// then on the committee, so `senator::*` counts it. A heartbeat alone
    /// does not make it connected.
    pub async fn heartbeat(&self, token: &str, agent: &str) -> reqwest::Response {
        self.request(
            Method::POST,
            &senate_core::api::heartbeat_path(&committee()),
            token,
        )
        .json(&Heartbeat {
            agent: path(agent),
            sessions: 1,
            cursor: Seq::ZERO,
        })
        .send()
        .await
        .unwrap()
    }

    /// Act as `senator` with `token`'s person by reading a cursor, which
    /// seats an agent and writes nothing to the log.
    pub async fn act_as(&self, token: &str, senator: &str) -> StatusCode {
        self.request(
            Method::GET,
            &senate_core::api::cursor_path(&committee(), "read"),
            token,
        )
        .query(&[("senator", senator)])
        .send()
        .await
        .unwrap()
        .status()
    }

    /// Append raw JSON (for what [`NewEvent`] cannot express, such as a
    /// path inside a name).
    pub async fn post_json(&self, token: &str, body: &serde_json::Value) -> reqwest::Response {
        self.request(
            Method::POST,
            &senate_core::api::events_path(&committee()),
            token,
        )
        .json(body)
        .send()
        .await
        .unwrap()
    }

    /// Append as alice over HTTP and return the stored event.
    pub async fn append(&self, body: &str) -> Event {
        let response = self.post_event(ALICE, &note(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let appended: AppendResponse = response.json().await.unwrap();
        assert!(!appended.duplicate);
        appended.event
    }

    /// Open the committee's SSE stream as `token`'s person, only reading:
    /// no agent's presence follows.
    pub async fn stream(&self, token: &str, after: Option<u64>, last_event_id: Option<u64>) -> Sse {
        self.stream_as(token, None, after, last_event_id).await
    }

    /// Open the stream as the connection of `senator` (e.g. `claude`).
    pub async fn stream_as(
        &self,
        token: &str,
        senator: Option<&str>,
        after: Option<u64>,
        last_event_id: Option<u64>,
    ) -> Sse {
        let response = self.try_stream(token, senator, after, last_event_id).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        Sse(Box::pin(response.bytes_stream().eventsource()))
    }

    /// The raw response of a stream request.
    pub async fn try_stream(
        &self,
        token: &str,
        senator: Option<&str>,
        after: Option<u64>,
        last_event_id: Option<u64>,
    ) -> reqwest::Response {
        let mut query = Vec::new();
        if let Some(after) = after {
            query.push(format!("after={after}"));
        }
        if let Some(senator) = senator {
            query.push(format!("senator={senator}"));
        }
        let mut path = senate_core::api::stream_path(&committee());
        if !query.is_empty() {
            path.push_str(&format!("?{}", query.join("&")));
        }
        let mut request = self.request(Method::GET, &path, token);
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id.to_string());
        }
        request.send().await.unwrap()
    }
}

/// A `note` from `senator::alice`: something alice's bridge may post. (`chat` is
/// platform-originated and only the server's own components append it.)
pub fn note(body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Note, person_addr("alice"), body)
}

type Frames = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            > + Send,
    >,
>;

/// An open SSE response.
pub struct Sse(pub Frames);

/// One SSE frame, decoded.
#[derive(Debug)]
pub enum Frame {
    Event(Box<Event>),
    Ping(String),
}

impl Sse {
    /// The next frame; `None` once the server ended the response.
    pub async fn frame(&mut self) -> Option<Frame> {
        let next = tokio::time::timeout(WAIT, self.0.next())
            .await
            .expect("timed out waiting for an SSE frame");
        match next? {
            Ok(frame) if frame.event == senate_core::api::SSE_PING => Some(Frame::Ping(frame.data)),
            Ok(frame) => {
                assert_eq!(frame.event, senate_core::api::SSE_EVENT);
                let event: Event = serde_json::from_str(&frame.data).unwrap();
                assert_eq!(frame.id, event.seq.to_string(), "SSE id is the seq");
                Some(Frame::Event(Box::new(event)))
            }
            Err(_) => None,
        }
    }

    /// The next log event, skipping pings; `None` once the response ended.
    pub async fn event(&mut self) -> Option<Event> {
        loop {
            match self.frame().await? {
                Frame::Event(event) => return Some(*event),
                Frame::Ping(_) => continue,
            }
        }
    }

    pub async fn seqs(&mut self, n: usize) -> Vec<u64> {
        let mut seqs = Vec::with_capacity(n);
        for _ in 0..n {
            seqs.push(self.event().await.expect("stream ended early").seq.0);
        }
        seqs
    }
}

/// The response's [`senate_core::api::EPOCH_HEADER`].
pub fn epoch_of(response: &reqwest::Response) -> String {
    response.headers()[senate_core::api::EPOCH_HEADER]
        .to_str()
        .unwrap()
        .to_string()
}

pub async fn api_error(response: reqwest::Response, status: StatusCode, code: &str) -> ApiError {
    assert_eq!(response.status(), status);
    let error: ApiError = response.json().await.unwrap();
    assert_eq!(error.code, code, "{error:?}");
    error
}

/// Poll `check` until it holds or [`WAIT`] passes.
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn seqs(events: &[Event]) -> Vec<u64> {
    events.iter().map(|e| e.seq.0).collect()
}

pub const ZERO: Seq = Seq::ZERO;

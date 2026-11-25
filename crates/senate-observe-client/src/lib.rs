//! Small Observe v1 HTTP client with bounded framing and resumable follow.

use std::net::IpAddr;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::{Response, StatusCode, Url, header};
use senate_observe_protocol::{
    Cursor, EVENTS_PATH, EventFrame, EventHeader, MAX_FRAME_BYTES, MAX_STATE_BYTES, ResetFrame,
    STATE_PATH, Snapshot,
};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("expected a bound numeric loopback Observe URL")]
    Endpoint,
    #[error("Observe HTTP request failed")]
    Transport(#[from] reqwest::Error),
    #[error("Observe returned HTTP {0}")]
    Http(StatusCode),
    #[error("Observe returned HTTP {status} with a retry delay")]
    RetryAfter { status: StatusCode, delay: Duration },
    #[error("Observe returned invalid v1 data")]
    Protocol,
    #[error("Observe requires a new snapshot")]
    Reset,
}

pub struct Client {
    http: reqwest::Client,
    endpoint: Url,
    authorization: reqwest::header::HeaderValue,
}

pub enum Frame {
    Event(EventFrame),
    Unknown(Value, Cursor),
    Reset(ResetFrame),
}

pub enum Update {
    Snapshot(Box<Snapshot>),
    Frame(Box<Frame>),
}

type Chunks = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

pub struct Events {
    stream: Chunks,
    buffer: Vec<u8>,
}

impl Client {
    pub fn new(endpoint: &str, token: &str) -> Result<Self, Error> {
        let endpoint = Url::parse(endpoint).map_err(|_| Error::Endpoint)?;
        let loopback = endpoint
            .host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if endpoint.scheme() != "http"
            || !loopback
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.path() != "/"
        {
            return Err(Error::Endpoint);
        }
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::Protocol)?;
        authorization.set_sensitive(true);
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            endpoint,
            authorization,
        })
    }

    async fn get(&self, path: &str, resume: Option<&Cursor>) -> Result<Response, Error> {
        let mut request = self
            .http
            .get(self.endpoint.join(path).map_err(|_| Error::Endpoint)?)
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone());
        if let Some(cursor) = resume {
            request = request.header("last-event-id", cursor.as_str());
        }
        let response = tokio::time::timeout(Duration::from_secs(15), request.send())
            .await
            .map_err(|_| Error::Protocol)??;
        match response.status() {
            StatusCode::OK => Ok(response),
            StatusCode::CONFLICT => Err(Error::Reset),
            status => {
                let delay = if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    response
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|value| retry_after(value.to_str().ok()?, SystemTime::now()))
                } else {
                    None
                };
                Err(
                    delay.map_or(Error::Http(status), |delay| Error::RetryAfter {
                        status,
                        delay,
                    }),
                )
            }
        }
    }

    pub async fn snapshot(&self) -> Result<Snapshot, Error> {
        let mut response = self.get(STATE_PATH, None).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = tokio::time::timeout(Duration::from_secs(15), response.chunk())
            .await
            .map_err(|_| Error::Protocol)??
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_STATE_BYTES {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }

    pub async fn events(&self, resume: Option<&Cursor>) -> Result<Events, Error> {
        let response = self.get(EVENTS_PATH, resume).await?;
        Ok(Events {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
        })
    }

    pub fn follow(&self) -> Follow<'_> {
        Follow {
            client: self,
            events: None,
            cursor: None,
            state: None,
            failures: 0,
        }
    }
}

const MAX_RETRY: Duration = Duration::from_secs(60);

fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(Duration::from_secs(seconds).min(MAX_RETRY));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default().min(MAX_RETRY))
}

fn retry_delay(failures: u8, hint: Option<Duration>, jitter: u8) -> Duration {
    let backoff = Duration::from_millis(200 * (1u64 << failures.min(3)));
    backoff
        .max(hint.unwrap_or_default())
        .saturating_add(Duration::from_millis(jitter.into()))
        .min(MAX_RETRY)
}

impl Events {
    pub async fn next(&mut self) -> Result<Option<Frame>, Error> {
        loop {
            if let Some(end) = delimiter(&self.buffer) {
                if end > MAX_FRAME_BYTES + 128 {
                    return Err(Error::Protocol);
                }
                let event: Vec<_> = self.buffer.drain(..end).collect();
                if let Some(event) = parse_event(&event)? {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.buffer.len() > MAX_FRAME_BYTES + 128 {
                return Err(Error::Protocol);
            }
            match tokio::time::timeout(Duration::from_secs(25), self.stream.next()).await {
                Ok(Some(Ok(chunk))) => self.buffer.extend_from_slice(&chunk),
                Ok(Some(Err(error))) => return Err(error.into()),
                Ok(None) => return Ok(None),
                Err(_) => return Err(Error::Protocol),
            }
        }
    }
}

fn delimiter(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).enumerate().find_map(|(index, pair)| {
        if pair == b"\n\n" {
            Some(index + 2)
        } else if pair == b"\r\n" && bytes.get(index + 2..index + 4) == Some(b"\r\n") {
            Some(index + 4)
        } else {
            None
        }
    })
}

fn parse_event(raw: &[u8]) -> Result<Option<Frame>, Error> {
    let raw = std::str::from_utf8(raw).map_err(|_| Error::Protocol)?;
    let mut id = None;
    let mut data = Vec::new();
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("id:") {
            if id
                .replace(value.strip_prefix(' ').unwrap_or(value))
                .is_some()
            {
                return Err(Error::Protocol);
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let data = data.join("\n");
    let header: EventHeader = serde_json::from_str(&data).map_err(|_| Error::Protocol)?;
    if header.kind == "stream.reset" {
        if id.is_some() || header.cursor.is_some() {
            return Err(Error::Protocol);
        }
        return serde_json::from_str(&data)
            .map(Frame::Reset)
            .map(Some)
            .map_err(|_| Error::Protocol);
    }
    let cursor = header.cursor.ok_or(Error::Protocol)?;
    if id != Some(cursor.as_str()) {
        return Err(Error::Protocol);
    }
    match serde_json::from_str::<EventFrame>(&data) {
        Ok(event) => Ok(Some(Frame::Event(event))),
        Err(error) if error.to_string().starts_with("unknown variant") => {
            serde_json::from_str(&data)
                .map(|data| Some(Frame::Unknown(data, cursor)))
                .map_err(|_| Error::Protocol)
        }
        Err(_) => Err(Error::Protocol),
    }
}

pub struct Follow<'a> {
    client: &'a Client,
    events: Option<Events>,
    cursor: Option<Cursor>,
    state: Option<Snapshot>,
    failures: u8,
}

impl Follow<'_> {
    pub fn state(&self) -> Option<&Snapshot> {
        self.state.as_ref()
    }

    pub async fn next(&mut self) -> Result<Update, Error> {
        loop {
            if self.cursor.is_none() {
                let snapshot = match self.client.snapshot().await {
                    Ok(snapshot) => snapshot,
                    Err(Error::Http(status))
                        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS =>
                    {
                        self.retry(None).await;
                        continue;
                    }
                    Err(Error::RetryAfter { delay, .. }) => {
                        self.retry(Some(delay)).await;
                        continue;
                    }
                    Err(Error::Transport(_)) => {
                        self.retry(None).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                self.cursor = Some(snapshot.cursor.clone());
                self.state = Some(snapshot.clone());
                return Ok(Update::Snapshot(Box::new(snapshot)));
            }
            if self.events.is_none() {
                match self.client.events(self.cursor.as_ref()).await {
                    Ok(events) => self.events = Some(events),
                    Err(Error::Reset) => {
                        self.cursor = None;
                        continue;
                    }
                    Err(Error::Http(status))
                        if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS =>
                    {
                        return Err(Error::Http(status));
                    }
                    Err(Error::RetryAfter { delay, .. }) => {
                        self.retry(Some(delay)).await;
                        continue;
                    }
                    Err(_) => {
                        self.retry(None).await;
                        continue;
                    }
                }
            }
            let Some(events) = &mut self.events else {
                continue;
            };
            match events.next().await {
                Ok(Some(Frame::Reset(_))) => {
                    self.events = None;
                    self.cursor = None;
                }
                Ok(Some(frame)) => {
                    let cursor = match &frame {
                        Frame::Event(event) => &event.cursor,
                        Frame::Unknown(_, cursor) => cursor,
                        Frame::Reset(_) => return Err(Error::Protocol),
                    };
                    if Some(cursor) == self.cursor.as_ref() {
                        continue;
                    }
                    self.cursor = Some(cursor.clone());
                    if let Some(state) = &mut self.state {
                        if let Frame::Event(event) = &frame {
                            senate_observe_protocol::recording::apply_frame(state, event);
                        } else {
                            state.cursor = cursor.clone();
                        }
                    }
                    self.failures = 0;
                    return Ok(Update::Frame(Box::new(frame)));
                }
                Err(Error::Protocol) => {
                    self.events = None;
                    self.cursor = None;
                    self.retry(None).await;
                }
                Ok(None) | Err(_) => {
                    self.events = None;
                    self.retry(None).await;
                }
            }
        }
    }

    async fn retry(&mut self, hint: Option<Duration>) {
        let mut entropy = [0];
        let _ = getrandom::fill(&mut entropy);
        tokio::time::sleep(retry_delay(self.failures, hint, entropy[0])).await;
        self.failures = self.failures.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_accepts_future_events_and_rejects_a_mismatched_id() -> Result<(), Error> {
        let cursor = "AAAAAAAAAAAAAAAAAAAAAA";
        let payload = format!(
            r#"{{"schema_version":1,"cursor":"{cursor}","at":"2026-11-25T00:00:00Z","kind":"future.event","data":{{}}}}"#
        );
        let text = format!("id: {cursor}\r\ndata: {payload}\r\n\r\n");
        assert_eq!(delimiter(text.as_bytes()), Some(text.len()));
        assert!(matches!(
            parse_event(text.as_bytes())?,
            Some(Frame::Unknown(_, _))
        ));
        assert!(parse_event(format!("id: invalid\ndata: {payload}\n\n").as_bytes()).is_err());
        assert!(parse_event(b": heartbeat\n\n")?.is_none());
        Ok(())
    }

    #[test]
    fn credentials_are_only_used_with_an_explicit_loopback_endpoint() {
        for endpoint in [
            "http://example.test:8765",
            "http://127.0.0.1:8765/?secret=x",
            "http://localhost:8765",
        ] {
            assert!(matches!(
                Client::new(endpoint, "fixture"),
                Err(Error::Endpoint)
            ));
        }
    }
    #[test]
    fn retry_after_honors_delta_and_http_date_with_bounded_jitter() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784111777);
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:42 GMT", now),
            Some(Duration::from_secs(5))
        );
        assert_eq!(retry_after("99999999999999999999999", now), Some(MAX_RETRY));
        assert_eq!(retry_after("-2", now), None);
        assert_eq!(
            retry_delay(0, retry_after("2", now), 91),
            Duration::from_millis(2091)
        );
        assert_eq!(retry_delay(0, Some(MAX_RETRY), 255), MAX_RETRY);
        assert_eq!(retry_delay(1, None, 0), Duration::from_millis(400));
    }
}

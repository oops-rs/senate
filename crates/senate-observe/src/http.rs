use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::HttpBody;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use senate_observe_protocol::{Cursor, EVENTS_PATH, ErrorBody, ErrorResponse, STATE_PATH};

use crate::grants::AuthError;
use crate::{ProjectionError, Registry};

const MAX_HEADERS_BYTES: usize = 8 * 1024;
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

pub fn router(registry: Arc<Registry>) -> Router {
    Router::new()
        .route(STATE_PATH, any(observe))
        .route(EVENTS_PATH, any(observe))
        .with_state(registry)
}

async fn observe(State(registry): State<Arc<Registry>>, request: Request) -> Response {
    let headers = request.headers();
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>();
    if !peer.is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback())
        || !valid_authority(&request, registry.address())
    {
        registry.audit.denied();
        return failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Observe requires its bound loopback authority",
        );
    }
    if request.uri().query().is_some()
        || headers.contains_key(header::COOKIE)
        || headers.contains_key(header::TRANSFER_ENCODING)
        || !request.body().is_end_stream()
        || headers
            .iter()
            .map(|(key, value)| key.as_str().len() + value.len() + 4)
            .sum::<usize>()
            > MAX_HEADERS_BYTES
    {
        registry.audit.denied();
        return bad_request();
    }
    let Ok(origin) = single(headers, &header::ORIGIN) else {
        registry.audit.denied();
        return bad_request();
    };

    if request.method() == Method::OPTIONS {
        return preflight(&registry, headers, origin);
    }
    if request.method() != Method::GET {
        let mut response = failure(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Observe accepts GET and OPTIONS",
        );
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, OPTIONS"));
        return response;
    }
    let stream = request.uri().path() == EVENTS_PATH;
    let credential = match single(headers, &header::AUTHORIZATION) {
        Ok(Some(value)) => value.strip_prefix("Bearer ").unwrap_or_default(),
        Ok(None) => "",
        Err(()) => return bad_request(),
    };
    let grant = match registry.authenticate(credential, origin, stream) {
        Ok(grant) => grant,
        Err(error) => {
            registry.audit.denied();
            let response = auth_failure(error);
            return if matches!(error, AuthError::AuthenticatedRate) {
                cors(response, origin, false)
            } else {
                response
            };
        }
    };
    if !grant.is_live() {
        return auth_failure(AuthError::Unauthorized);
    }
    let cursor = match single(headers, &LAST_EVENT_ID) {
        Ok(None) => None,
        Ok(Some(value)) if stream => match value.parse::<Cursor>() {
            Ok(cursor) => Some(cursor),
            Err(_) => return cors(bad_request(), origin, false),
        },
        _ => return cors(bad_request(), origin, false),
    };
    let id = grant.info.id.clone();
    let result = if stream {
        registry
            .projections
            .events(grant, cursor, registry.audit.clone())
            .await
    } else {
        registry
            .projections
            .snapshot(&grant)
            .await
            .and_then(|json| {
                if !grant.is_live() {
                    return Err(ProjectionError::Unauthorized);
                }
                Ok((
                    [(header::CONTENT_TYPE, "application/json")],
                    json.to_string(),
                )
                    .into_response())
            })
    };
    let response = match result {
        Ok(response) => {
            registry.audit.record(
                if stream { "events" } else { "state" },
                if stream { "opened" } else { "read" },
                Some(&id),
            );
            response
        }
        Err(error) => {
            if matches!(error, ProjectionError::Reset) {
                registry.audit.record("events", "reset", Some(&id));
            }
            projection_failure(error)
        }
    };
    cors(response, origin, false)
}

fn preflight(registry: &Registry, headers: &HeaderMap, origin: Option<&str>) -> Response {
    let Some(origin) = origin else {
        return bad_request();
    };
    let allowed_headers = single(headers, &header::ACCESS_CONTROL_REQUEST_HEADERS);
    if !matches!(
        single(headers, &header::ACCESS_CONTROL_REQUEST_METHOD),
        Ok(Some("GET"))
    ) || headers.contains_key(header::AUTHORIZATION)
        || !allowed_headers.is_ok_and(|value| {
            value.is_none_or(|value| {
                value.split(',').all(|name| {
                    name.trim().eq_ignore_ascii_case("authorization")
                        || name.trim().eq_ignore_ascii_case("last-event-id")
                })
            })
        })
    {
        registry.audit.denied();
        return bad_request();
    }
    match registry.preflight(origin) {
        Ok(()) => cors(StatusCode::NO_CONTENT.into_response(), Some(origin), true),
        Err(error) => {
            registry.audit.denied();
            auth_failure(error)
        }
    }
}

fn valid_authority(request: &Request, bound: SocketAddr) -> bool {
    let Ok(host) = single(request.headers(), &header::HOST) else {
        return false;
    };
    let authority = request
        .uri()
        .authority()
        .map(|authority| authority.as_str());
    (host.is_some() || authority.is_some())
        && [host, authority]
            .into_iter()
            .flatten()
            .all(|value| value.parse::<SocketAddr>() == Ok(bound))
}

fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, ()> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values
        .next()
        .map(HeaderValue::to_str)
        .transpose()
        .map_err(|_| ())?;
    if values.next().is_some() {
        return Err(());
    }
    Ok(value)
}

fn cors(mut response: Response, origin: Option<&str>, preflight: bool) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::VARY,
        HeaderValue::from_static(if preflight {
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
        } else {
            "Origin"
        }),
    );
    if let Some(origin) = origin.and_then(|value| HeaderValue::from_str(value).ok()) {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    if preflight {
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Authorization, Last-Event-ID"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("60"),
        );
    }
    response
}

fn auth_failure(error: AuthError) -> Response {
    match error {
        AuthError::Unauthorized => {
            let mut response = failure(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "A valid observer bearer is required",
            );
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            response
        }
        AuthError::Origin => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "This origin is outside the observer grant",
        ),
        AuthError::Rate | AuthError::AuthenticatedRate => {
            let mut response = failure(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "Observe request limit reached",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
            response
        }
    }
}

fn projection_failure(error: ProjectionError) -> Response {
    match error {
        ProjectionError::NotReady => {
            let mut response = failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "not_ready",
                "Observe projection is not ready",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
            response
        }
        ProjectionError::TooLarge => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "view_too_large",
            "Use an observer grant with fewer committees",
        ),
        ProjectionError::Reset => failure(
            StatusCode::CONFLICT,
            "resync_required",
            "Fetch a new Observe snapshot",
        ),
        ProjectionError::RateLimited => auth_failure(AuthError::AuthenticatedRate),
        ProjectionError::Unauthorized => auth_failure(AuthError::Unauthorized),
    }
}

fn bad_request() -> Response {
    failure(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "Invalid Observe request",
    )
}

fn failure(status: StatusCode, code: &str, message: &str) -> Response {
    let mut response = (
        status,
        Json(ErrorResponse {
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;
    use crate::{CreateGrant, IssuedGrant, Scope};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn issue(registry: &Registry, origin: &str) -> Result<IssuedGrant, Box<dyn std::error::Error>> {
        Ok(registry.create(
            CreateGrant {
                name: "Scene test".into(),
                committees: None,
                all_committees: true,
                scope: Scope::Metadata,
                origins: vec![origin.into()],
                topology: false,
                expires_in_days: None,
            },
            Default::default(),
        )?)
    }

    fn request(path: &str) -> axum::http::request::Builder {
        axum::http::Request::builder()
            .uri(path)
            .header(header::HOST, "127.0.0.1:42567")
    }

    #[tokio::test]
    async fn browser_origin_is_checked_against_each_authenticated_grant() -> TestResult {
        let home = tempfile::tempdir()?;
        let address = "127.0.0.1:42567".parse()?;
        let registry = Registry::open(home.path(), address)?;
        let first = issue(&registry, "https://scene.example")?;
        let _second = issue(&registry, "https://other.example")?;
        let app = router(registry).layer(axum::Extension(ConnectInfo(address)));
        let options = app
            .clone()
            .oneshot(
                request(EVENTS_PATH)
                    .method(Method::OPTIONS)
                    .header(header::ORIGIN, "https://other.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "authorization,last-event-id",
                    )
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(options.status(), 204);
        assert_eq!(
            options.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://other.example"
        );
        assert!(
            !options
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
        for (origin, expected) in [
            ("https://scene.example", 503),
            ("https://other.example", 403),
            ("null", 403),
        ] {
            let response = app
                .clone()
                .oneshot(
                    request(EVENTS_PATH)
                        .header(header::ORIGIN, origin)
                        .header(header::AUTHORIZATION, format!("Bearer {}", first.token))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), expected);
            assert_eq!(
                response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN),
                expected == 503
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn refuses_ambiguous_authority_non_loopback_head_body_query_and_cookies() -> TestResult {
        let home = tempfile::tempdir()?;
        let address = "127.0.0.1:42567".parse()?;
        let registry = Registry::open(home.path(), address)?;
        let app = router(registry.clone()).layer(axum::Extension(ConnectInfo(address)));
        let base = || request(STATE_PATH);
        let cases = [
            (
                base()
                    .header(header::HOST, "rebinding.example:42567")
                    .body(Body::empty())?,
                403,
            ),
            (
                axum::http::Request::builder()
                    .uri(STATE_PATH)
                    .header(header::HOST, "rebinding.example:42567")
                    .body(Body::empty())?,
                403,
            ),
            (base().method(Method::HEAD).body(Body::empty())?, 405),
            (base().method(Method::POST).body(Body::empty())?, 405),
            (base().body(Body::from("a payload"))?, 400),
            (
                base()
                    .header(header::COOKIE, "auth=untrusted")
                    .body(Body::empty())?,
                400,
            ),
            (
                request("/v1/observe/state?token=query-bearer").body(Body::empty())?,
                400,
            ),
            (
                base()
                    .header(header::AUTHORIZATION, "Bearer x")
                    .header(header::AUTHORIZATION, "Bearer y")
                    .body(Body::empty())?,
                400,
            ),
        ];
        for (request, status) in cases {
            assert_eq!(app.clone().oneshot(request).await?.status(), status);
        }
        let remote: SocketAddr = "192.0.2.2:12345".parse()?;
        let remote = router(registry).layer(axum::Extension(ConnectInfo(remote)));
        assert_eq!(
            remote.oneshot(base().body(Body::empty())?).await?.status(),
            403
        );
        Ok(())
    }

    #[tokio::test]
    async fn authenticated_requests_are_limited_per_grant() -> TestResult {
        let home = tempfile::tempdir()?;
        let address = "127.0.0.1:42567".parse()?;
        let registry = Registry::open(home.path(), address)?;
        let grant = issue(&registry, "https://scene.example")?;
        let app = router(registry).layer(axum::Extension(ConnectInfo(address)));
        for expected in [503, 503, 503, 503, 429] {
            let response = app
                .clone()
                .oneshot(
                    request(STATE_PATH)
                        .header(header::ORIGIN, "https://scene.example")
                        .header(header::AUTHORIZATION, format!("Bearer {}", grant.token))
                        .body(Body::empty())?,
                )
                .await?;
            assert_eq!(response.status(), expected);
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                "https://scene.example"
            );
        }
        Ok(())
    }
}

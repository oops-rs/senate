use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use senate_core::api::{ApiError, code};
use senate_core::{HubError, Locator, SenatorName, Seq};

/// A non-2xx response with an [`ApiError`] body.
#[derive(Debug)]
pub(crate) struct ApiFailure {
    status: StatusCode,
    code: &'static str,
    message: String,
    /// With `ambiguous_recipient`: the agents to choose from.
    candidates: Vec<SenatorName>,
    /// With `ambiguous_name`: the qualified names to choose from.
    locators: Vec<Locator>,
}

impl ApiFailure {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            candidates: Vec::new(),
            locators: Vec::new(),
        }
    }

    /// A qualified name several things answer to: 409 with them.
    #[expect(
        dead_code,
        reason = "no route answers ambiguous_name yet: GET /v1/resolve answers for this Senate alone"
    )]
    pub(crate) fn ambiguous_name(message: impl Into<String>, locators: Vec<Locator>) -> Self {
        Self {
            locators,
            ..Self::new(StatusCode::CONFLICT, code::AMBIGUOUS_NAME, message)
        }
    }

    pub(crate) fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            code::UNAUTHORIZED,
            "missing or unknown bearer token",
        )
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code::FORBIDDEN, message)
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code::BAD_REQUEST, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code::NOT_FOUND, message)
    }

    /// The client's cursor is beyond the committee's head: it belongs to
    /// another epoch of the log (see `senate_core::api`).
    pub(crate) fn cursor_ahead(after: Seq, head: Seq) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            code::CURSOR_AHEAD,
            format!(
                "cursor {after} is beyond the committee's head {head}: it is from another \
                 epoch of the log (see the epoch header); drop it and start over"
            ),
        )
    }

    pub(crate) fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            code::BAD_REQUEST,
            "method not allowed on this path",
        )
    }
}

impl From<HubError> for ApiFailure {
    fn from(error: HubError) -> Self {
        match error {
            HubError::Rejected(message) => Self::bad_request(message),
            // Several agents: 409 with the candidates, so the sender names
            // one. No agent and nobody to take it instead: 400.
            HubError::Unresolved(error) => {
                let api = ApiError::from(&error);
                let status = if api.code == code::AMBIGUOUS_RECIPIENT {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::BAD_REQUEST
                };
                Self {
                    status,
                    code: if api.code == code::AMBIGUOUS_RECIPIENT {
                        code::AMBIGUOUS_RECIPIENT
                    } else {
                        code::BAD_REQUEST
                    },
                    message: api.message,
                    candidates: api.candidates,
                    locators: Vec::new(),
                }
            }
            HubError::NotFound(message) => Self::not_found(message),
            HubError::Unavailable(message) => {
                tracing::warn!(%message, "request failed: log unavailable");
                Self::new(StatusCode::SERVICE_UNAVAILABLE, code::INTERNAL, message)
            }
            HubError::Other(source) => {
                tracing::error!(error = %source, "request failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    code::INTERNAL,
                    "internal error",
                )
            }
        }
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        let body = Json(ApiError {
            code: self.code.to_string(),
            message: self.message,
            candidates: self.candidates,
            locators: self.locators,
        });
        let mut response = (self.status, body).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

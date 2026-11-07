//! API errors in the `/client/v4` envelope.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use selvedge_core::envelope::Envelope;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: u32,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: u32, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, 10021, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, 10007, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, 10000, "Authentication error")
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, 10000, "Authentication error")
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        // Internal details go to the log, not to the client.
        tracing::error!(error = %format!("{e:#}"), "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, 10013, "Internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(Envelope::error(self.code, self.message))).into_response()
    }
}

/// Wraps a successful result in the envelope.
pub fn ok(result: serde_json::Value) -> Response {
    Json(Envelope::ok(result)).into_response()
}

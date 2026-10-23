//! Stable JSON error envelope shared by every route.
//!
//! All non-2xx responses produced by this server use the shape:
//!
//! ```json
//! { "error": { "code": "EGRESS_DENIED", "message": "human readable explanation" } }
//! ```
//!
//! `code` values are part of the public API and are documented in `docs/API.md`;
//! clients should branch on the code, never on the message.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;

/// Inference engine disabled or failed to initialize.
pub const ENGINE_UNAVAILABLE: &str = "ENGINE_UNAVAILABLE";
/// The engine was reached but the inference call failed.
pub const INFERENCE_FAILED: &str = "INFERENCE_FAILED";
/// The request is missing a Bearer token.
pub const UNAUTHORIZED: &str = "UNAUTHORIZED";
/// The caller is not allowed to use this route (non-loopback admin or CORS).
pub const FORBIDDEN: &str = "FORBIDDEN";
/// No route matches the request path.
pub const NOT_FOUND: &str = "NOT_FOUND";
/// Request body (or a multipart part) exceeded the configured limit.
pub const PAYLOAD_TOO_LARGE: &str = "PAYLOAD_TOO_LARGE";
/// Request JSON could not be parsed.
pub const INVALID_JSON: &str = "INVALID_JSON";
/// Multipart body could not be parsed.
pub const INVALID_MULTIPART: &str = "INVALID_MULTIPART";
/// The request was syntactically valid but semantically unusable.
pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
/// The device terminal channel is disabled by configuration.
pub const DEVICE_DISABLED: &str = "DEVICE_DISABLED";
/// Network egress is disabled by configuration.
pub const EGRESS_DISABLED: &str = "EGRESS_DISABLED";
/// The upstream host is not in `egress.allowed_hosts`.
pub const EGRESS_DENIED: &str = "EGRESS_DENIED";
/// The upstream credential environment variable is missing or empty.
pub const EGRESS_NOT_CONFIGURED: &str = "EGRESS_NOT_CONFIGURED";
/// The upstream returned an error, timed out or was unreachable.
pub const EGRESS_UPSTREAM_ERROR: &str = "EGRESS_UPSTREAM_ERROR";
/// The upstream response exceeded `egress.max_response_bytes`.
pub const EGRESS_RESPONSE_TOO_LARGE: &str = "EGRESS_RESPONSE_TOO_LARGE";
/// Unexpected internal failure (including caught panics).
pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";

/// Serialized error envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorDetail,
}

/// Body of the `error` object.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// Build the JSON value of an error envelope.
pub fn error_json(code: &str, message: impl Into<String>) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message.into() } })
}

/// Build a complete error response with the stable envelope.
pub fn error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = ErrorEnvelope {
        error: ErrorDetail {
            code: code.to_string(),
            message: message.into(),
        },
    };
    (status, Json(body)).into_response()
}

/// `503 ENGINE_UNAVAILABLE` — inference engine disabled or not initialized.
pub fn engine_unavailable(message: impl Into<String>) -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, ENGINE_UNAVAILABLE, message)
}

/// `500 INTERNAL_ERROR` — caught panic or unexpected failure.
pub fn internal_error(message: impl Into<String>) -> Response {
    error_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR, message)
}

/// `401 UNAUTHORIZED` — missing or rejected Bearer token.
pub fn unauthorized(message: impl Into<String>) -> Response {
    error_response(StatusCode::UNAUTHORIZED, UNAUTHORIZED, message)
}

/// `403 FORBIDDEN` — policy denial.
pub fn forbidden(code: &str, message: impl Into<String>) -> Response {
    error_response(StatusCode::FORBIDDEN, code, message)
}

/// `413 PAYLOAD_TOO_LARGE` — a configured size cap was exceeded.
pub fn payload_too_large(message: impl Into<String>) -> Response {
    error_response(StatusCode::PAYLOAD_TOO_LARGE, PAYLOAD_TOO_LARGE, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_has_stable_shape() {
        let value = error_json(EGRESS_DENIED, "host not allowed");
        assert_eq!(value["error"]["code"], EGRESS_DENIED);
        assert_eq!(value["error"]["message"], "host not allowed");
        assert_eq!(value.as_object().unwrap().len(), 1);
    }
}

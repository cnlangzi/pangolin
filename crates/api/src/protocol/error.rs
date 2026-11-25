//! The error envelope of the API.
//!
//! Every failure answers with the same shape:
//!
//! ```json
//! { "error": { "code": "site_not_found", "message": "…", "field": null } }
//! ```
//!
//! `code` is stable and machine-readable — a dashboard branches on it,
//! never on the message. `message` is for the operator's log. `field`
//! names the offending input when there is one.

use serde::Serialize;

/// The stable error codes of the API.
pub mod codes {
    /// A header is missing or malformed, or the body is not valid JSON.
    pub const MALFORMED_REQUEST: &str = "malformed_request";
    /// The catch-all for an authentication failure.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// The request's timestamp is outside the ±300 s window.
    pub const TIMESTAMP_OUT_OF_WINDOW: &str = "timestamp_out_of_window";
    /// The bearer token does not match.
    pub const INVALID_TOKEN: &str = "invalid_token";
    /// The nonce was already used inside the window.
    pub const REPLAYED_NONCE: &str = "replayed_nonce";
    /// The Ed25519 signature does not verify.
    pub const INVALID_SIGNATURE: &str = "invalid_signature";
    /// The requested resource does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// The resource already exists.
    pub const CONFLICT: &str = "conflict";
    /// A field failed validation.
    pub const UNPROCESSABLE: &str = "unprocessable_entity";
    /// Something went wrong that the API did not expect.
    pub const INTERNAL_ERROR: &str = "internal_error";
}

/// An API error: the HTTP status, the stable code and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status code.
    pub status: u16,
    /// The stable, machine-readable code.
    pub code: String,
    /// The human-readable reason.
    pub message: String,
    /// The offending field, when the failure names one.
    pub field: Option<String>,
}

impl ApiError {
    /// Build an error from its parts.
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            field: None,
        }
    }

    /// Name the field the failure is about.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// 400 — the request itself is malformed.
    pub fn malformed(message: impl Into<String>) -> Self {
        Self::new(400, codes::MALFORMED_REQUEST, message)
    }

    /// 401 — the request could not be authenticated.
    pub fn unauthorized(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(401, code, message)
    }

    /// 404 — the resource does not exist.
    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(404, code, message)
    }

    /// 409 — the resource already exists.
    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(409, code, message)
    }

    /// 422 — a field failed validation.
    pub fn unprocessable(
        code: impl Into<String>,
        message: impl Into<String>,
        field: Option<&str>,
    ) -> Self {
        let e = Self::new(422, code, message);
        match field {
            Some(f) => e.with_field(f),
            None => e,
        }
    }

    /// 500 — an unexpected failure.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, codes::INTERNAL_ERROR, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// The JSON envelope of a failed request.
#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    field: Option<&'a str>,
}

/// Render an error as the JSON envelope.
pub fn error_body(err: &ApiError) -> Vec<u8> {
    let envelope = ErrorEnvelope {
        error: ErrorBody {
            code: &err.code,
            message: &err.message,
            field: err.field.as_deref(),
        },
    };
    serde_json::to_vec(&envelope).unwrap_or_else(|_| {
        br#"{"error":{"code":"internal_error","message":"the error could not be rendered","field":null}}"#.to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_carries_the_code_the_message_and_the_field() {
        let err = ApiError::unprocessable("backend_invalid", "Invalid backend", Some("backend"));
        let body = error_body(&err);
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["error"]["code"], "backend_invalid");
        assert_eq!(json["error"]["message"], "Invalid backend");
        assert_eq!(json["error"]["field"], "backend");
    }

    #[test]
    fn an_error_without_a_field_carries_a_null_one() {
        let err = ApiError::not_found("site_not_found", "no such site");
        let body = error_body(&err);
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert!(json["error"]["field"].is_null(), "{json}");
    }
}

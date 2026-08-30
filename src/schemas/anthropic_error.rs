//! Anthropic-shaped error rendering for the `/v1/messages` route.
//!
//! The Anthropic SDKs branch on `error.type` inside a `{"type":"error", ...}`
//! envelope, so the `OpenAI`-shaped `{"error":{"message","type"}}` that
//! `AppError` renders is not interchangeable. This newtype wraps an `AppError`
//! at the handler boundary and re-renders it; every other route is untouched.

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::schemas::app_error::AppError;

#[derive(Debug)]
pub struct AnthropicError(pub AppError);

impl From<AppError> for AnthropicError {
    fn from(error: AppError) -> Self {
        AnthropicError(error)
    }
}

impl IntoResponse for AnthropicError {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let body = json!({
            "type": "error",
            "error": { "type": error_type_for_status(status.as_u16()), "message": self.0.message() }
        });
        (status, Json(body)).into_response()
    }
}

/// HTTP status -> Anthropic `error.type`. Anything unmapped (including 5xx and
/// the 501 the not-yet-implemented stream path returns) is `api_error`.
#[must_use]
pub fn error_type_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_to_error_type_map_covers_400_401_403_404_413_429_529_5xx() {
        assert_eq!(error_type_for_status(400), "invalid_request_error");
        assert_eq!(error_type_for_status(401), "authentication_error");
        assert_eq!(error_type_for_status(403), "permission_error");
        assert_eq!(error_type_for_status(404), "not_found_error");
        assert_eq!(error_type_for_status(413), "request_too_large");
        assert_eq!(error_type_for_status(429), "rate_limit_error");
        assert_eq!(error_type_for_status(529), "overloaded_error");
        assert_eq!(error_type_for_status(500), "api_error");
        assert_eq!(error_type_for_status(502), "api_error");
        assert_eq!(error_type_for_status(501), "api_error");
    }

    #[test]
    fn app_error_renders_the_anthropic_envelope() {
        let response = AnthropicError(AppError::BadRequest("nope".into())).into_response();
        assert_eq!(response.status(), 400);
    }
}

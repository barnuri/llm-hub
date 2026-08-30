use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized,
    NotFound(String),
    NotImplemented(String),
    ReadOnly,
    Internal(String),
    UpstreamUnavailable(String),
}

impl AppError {
    fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            AppError::ReadOnly => StatusCode::FORBIDDEN,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::UpstreamUnavailable(_) => StatusCode::BAD_GATEWAY,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "invalid_request_error",
            AppError::Unauthorized => "authentication_error",
            AppError::NotFound(_) => "not_found_error",
            AppError::NotImplemented(_) => "not_implemented_error",
            AppError::ReadOnly => "read_only_error",
            AppError::Internal(_) => "internal_error",
            AppError::UpstreamUnavailable(_) => "upstream_error",
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::BadRequest(m)
            | AppError::NotFound(m)
            | AppError::NotImplemented(m)
            | AppError::Internal(m)
            | AppError::UpstreamUnavailable(m) => m.clone(),
            AppError::Unauthorized => "missing or invalid api key".to_string(),
            AppError::ReadOnly => "config is read-only (LLM_HUB_CONFIG_READONLY=true)".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "message": self.message(), "type": self.kind() } });
        (self.status(), Json(body)).into_response()
    }
}

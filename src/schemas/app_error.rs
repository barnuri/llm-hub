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
    Conflict(String),
    Internal(String),
    UpstreamUnavailable(String),
}

impl AppError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            AppError::ReadOnly => StatusCode::FORBIDDEN,
            AppError::Conflict(_) => StatusCode::CONFLICT,
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
            AppError::Conflict(_) => "conflict_error",
            AppError::Internal(_) => "internal_error",
            AppError::UpstreamUnavailable(_) => "upstream_error",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            AppError::BadRequest(m)
            | AppError::NotFound(m)
            | AppError::NotImplemented(m)
            | AppError::Internal(m)
            | AppError::UpstreamUnavailable(m)
            | AppError::Conflict(m) => m.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn render(error: AppError) -> (StatusCode, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn every_variant_renders_its_status_and_error_type() {
        let cases = [
            (
                AppError::BadRequest("bad".into()),
                400,
                "invalid_request_error",
            ),
            (AppError::Unauthorized, 401, "authentication_error"),
            (AppError::NotFound("gone".into()), 404, "not_found_error"),
            (
                AppError::NotImplemented("later".into()),
                501,
                "not_implemented_error",
            ),
            (AppError::ReadOnly, 403, "read_only_error"),
            (AppError::Conflict("taken".into()), 409, "conflict_error"),
            (AppError::Internal("boom".into()), 500, "internal_error"),
            (
                AppError::UpstreamUnavailable("down".into()),
                502,
                "upstream_error",
            ),
        ];

        for (error, status, kind) in cases {
            let (actual_status, body) = render(error).await;

            assert_eq!(actual_status.as_u16(), status);
            assert_eq!(body["error"]["type"], kind);
        }
    }

    #[tokio::test]
    async fn a_variant_with_a_message_renders_that_message() {
        let (_, body) = render(AppError::Conflict("name already taken".into())).await;

        assert_eq!(
            body,
            json!({ "error": { "message": "name already taken", "type": "conflict_error" } })
        );
    }

    #[test]
    fn unauthorized_and_read_only_carry_fixed_messages() {
        assert_eq!(
            AppError::Unauthorized.message(),
            "missing or invalid api key"
        );
        assert_eq!(
            AppError::ReadOnly.message(),
            "config is read-only (LLM_HUB_CONFIG_READONLY=true)"
        );
    }
}

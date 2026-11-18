use serde::Serialize;
use utoipa::ToSchema;

/// Error envelope the hub returns on a failed `/api/*` or `/v1/*` call.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorDetail {
    /// Human-readable message.
    pub message: String,
    /// Error kind, for example `invalid_request_error` or `authentication_error`.
    #[serde(rename = "type")]
    pub kind: String,
}

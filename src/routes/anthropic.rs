//! `POST /v1/messages` — the Anthropic Messages API over an `OpenAI`-compatible
//! upstream.
//!
//! The route translates the body and then hands off to the ordinary proxy
//! `attempt_loop`, so the fallback chain, retry policy, per-attempt timeout and
//! the `x-llm-hub-*` control headers all work here exactly as they do on
//! `/v1/chat/completions`.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method};
use axum::response::Response;

use crate::dependencies::state::AppState;
use crate::routes::proxy::{attempt_loop, read_body_capped};
use crate::schemas::anthropic_error::AnthropicError;
use crate::schemas::app_error::AppError;
use crate::services::transforms::RouteKind;

/// Anthropic-only request headers. Stripped here rather than in
/// `utils::headers` so the `/v1/{*path}` passthrough — which may legitimately
/// front a real Anthropic-shaped upstream — keeps forwarding them.
const ANTHROPIC_REQUEST_HEADERS: [&str; 3] = [
    "anthropic-version",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
];

/// `stream: true` needs nothing here: the request is translated and forwarded
/// like any other, and the winning response is recognised as SSE by its
/// content-type and translated frame by frame on the way back.
///
/// # Errors
/// 400 on a malformed body, an unprefixed model, an unsupported image source,
/// or a body over `LLM_HUB_MAX_REPLAY_BYTES`; 502 when every attempt fails.
pub async fn messages(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, AnthropicError> {
    let (parts, body) = req.into_parts();

    let content_type = parts
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("multipart/") {
        return Err(AppError::NotImplemented(
            "multipart bodies are not supported on /v1/messages".to_string(),
        )
        .into());
    }

    // Translation needs the whole document, so there is no honest oversized
    // passthrough mode here — over the cap is a 400, not a silent degrade.
    let (buffered, overflow) = read_body_capped(body, state.config().max_replay_bytes).await?;
    if overflow.is_some() {
        return Err(AppError::BadRequest(
            "anthropic request body exceeds LLM_HUB_MAX_REPLAY_BYTES; raise it or use /v1/chat/completions"
                .to_string(),
        )
        .into());
    }

    let headers = strip_anthropic_request_headers(&parts.headers);
    Ok(attempt_loop(
        &state,
        Method::POST,
        "/v1/messages",
        &headers,
        buffered,
        RouteKind::Anthropic,
    )
    .await?)
}

fn strip_anthropic_request_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if ANTHROPIC_REQUEST_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn strips_anthropic_only_request_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert("anthropic-beta", HeaderValue::from_static("tools-2024"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let out = strip_anthropic_request_headers(&headers);
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("content-type"));
    }
}

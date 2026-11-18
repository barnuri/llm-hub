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

/// Anthropic-only request headers, stripped here so the `/v1/{*path}` passthrough keeps
/// them. `anthropic-beta` stays: the attempt loop sends it on native attempts only.
const ANTHROPIC_REQUEST_HEADERS: [&str; 2] = [
    "anthropic-version",
    "anthropic-dangerous-direct-browser-access",
];

/// `stream: true` needs nothing here: the request is translated and forwarded
/// like any other, and the winning response is recognised as SSE by its
/// content-type and translated frame by frame on the way back.
///
/// # Errors
/// 400 on a malformed body, an unprefixed model, an unsupported image source,
/// or a body over `LLM_HUB_MAX_REPLAY_BYTES`; 502 when every attempt fails.
#[utoipa::path(post, path = "/v1/messages", tag = "proxy", summary = "Anthropic Messages",
    description = "Anthropic Messages API. The model must be `<profile>/<model>`. The hub forwards natively to Anthropic-capable upstreams and translates for the rest. Fallback and transform headers (`X-LLM-Hub-*`) apply. The hub reports errors in the Anthropic error envelope.",
    request_body(content = Object, description = "Anthropic Messages request body", content_type = "application/json"),
    responses((status = 200, description = "Message, or an SSE stream when `stream` is true", content((Object = "application/json"), (String = "text/event-stream"))),
        (status = 400, description = "Malformed body, unprefixed model, unsupported image source or oversized body.", body = Object),
        (status = 401, description = "Missing or invalid hub key.", body = Object),
        (status = 502, description = "Every attempt failed.", body = Object)))]
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
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use axum::body::Body;
    use axum::http::{HeaderValue, StatusCode};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Arc;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DEAD_BASE_URL: &str = "http://127.0.0.1:9";

    fn state_with(base_url: &str, extra: &[(&str, &str)]) -> AppState {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "solo"),
            ("LLM_HUB_SOLO_BASE_URL", base_url),
        ]
        .iter()
        .chain(extra)
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn request(content_type: &str, body: impl Into<Body>) -> Request {
        Request::builder()
            .method(Method::POST)
            .uri("/v1/messages")
            .header("content-type", content_type)
            .header("anthropic-version", "2023-06-01")
            .body(body.into())
            .unwrap()
    }

    fn messages_body(model: &str) -> String {
        json!({
            "model": model,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string()
    }

    async fn json_of(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn refusal(state: AppState, req: Request) -> (StatusCode, Value) {
        let Err(error) = messages(State(state), req).await else {
            panic!("the request was served, not refused");
        };
        let response = axum::response::IntoResponse::into_response(error);
        (response.status(), json_of(response).await)
    }

    #[tokio::test]
    async fn a_multipart_body_is_not_implemented() {
        let state = state_with(DEAD_BASE_URL, &[]);

        let (status, body) =
            refusal(state, request("Multipart/Form-Data; boundary=x", "--x--")).await;

        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["type"], "error");
        assert_eq!(
            body["error"]["message"],
            "multipart bodies are not supported on /v1/messages"
        );
    }

    #[tokio::test]
    async fn a_body_over_the_replay_cap_is_a_bad_request() {
        let state = state_with(DEAD_BASE_URL, &[("LLM_HUB_MAX_REPLAY_BYTES", "16")]);

        let (status, body) = refusal(
            state,
            request("application/json", messages_body("solo/gpt-main")),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("LLM_HUB_MAX_REPLAY_BYTES")
        );
    }

    #[tokio::test]
    async fn an_unprefixed_model_is_refused_in_the_anthropic_envelope() {
        let state = state_with(DEAD_BASE_URL, &[]);

        let (status, body) = refusal(
            state,
            request("application/json", messages_body("gpt-main")),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn a_message_is_translated_upstream_and_back_without_anthropic_only_headers() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(json!({
                "model": "gpt-main",
                "messages": [{"role": "user", "content": "hi"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-7", "object": "chat.completion",
                "choices": [{"index": 0, "finish_reason": "stop",
                    "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 6, "total_tokens": 11}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let state = state_with(&upstream.uri(), &[]);

        let response = messages(
            State(state),
            request("application/json", messages_body("solo/gpt-main")),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_of(response).await;
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"][0]["text"], "ok");
        assert_eq!(body["stop_reason"], "end_turn");
        let sent = upstream.received_requests().await.unwrap();
        assert!(
            sent.iter()
                .all(|req| !req.headers.contains_key("anthropic-version"))
        );
    }

    #[test]
    fn strips_anthropic_only_request_headers_but_keeps_the_beta() {
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert("anthropic-beta", HeaderValue::from_static("tools-2024"));
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let out = strip_anthropic_request_headers(&headers);
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("content-type"));
        assert_eq!(out["anthropic-beta"], "tools-2024");
    }
}

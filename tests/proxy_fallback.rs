//! Fallback loop: 429 fails over, 400 never retries, attempts trail reported.

use std::collections::HashMap;
use std::time::Duration;

use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "support/hub.rs"]
mod hub;

use hub::{start_alpha_beta_hub, start_hub};

fn chat_body(model: &str) -> serde_json::Value {
    serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

fn chat_body_with(model: &str, text: &str) -> serde_json::Value {
    serde_json::json!({"model": model, "messages": [{"role": "user", "content": text}]})
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }))
}

#[tokio::test]
async fn rate_limited_primary_fails_over() {
    let limited = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&limited)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(1)
        .mount(&healthy)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "limited,healthy".into()),
        ("LLM_HUB_LIMITED_BASE_URL".into(), limited.uri()),
        ("LLM_HUB_HEALTHY_BASE_URL".into(), healthy.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-fallbacks", "healthy/gpt-fallback")
        .json(&chat_body("limited/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-model"],
        "healthy/gpt-fallback"
    );
    let attempts = response.headers()["x-llm-hub-attempts"].to_str().unwrap();
    assert!(
        attempts.contains("limited/gpt-main=429"),
        "attempts trail: {attempts}"
    );
    assert!(
        attempts.contains("healthy/gpt-fallback=200"),
        "attempts trail: {attempts}"
    );
    drop(hub);
}

#[tokio::test]
async fn bad_request_never_retries() {
    let rejecting = MockServer::start().await;
    let never_called = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": {"message": "bad"}})),
        )
        .expect(1)
        .mount(&rejecting)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(0)
        .mount(&never_called)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "rejecting,fallback".into()),
        ("LLM_HUB_REJECTING_BASE_URL".into(), rejecting.uri()),
        ("LLM_HUB_FALLBACK_BASE_URL".into(), never_called.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-fallbacks", "fallback/gpt-x")
        .json(&chat_body("rejecting/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        400,
        "request-shaped errors pass through untouched"
    );
    assert_eq!(response.headers()["x-llm-hub-model"], "rejecting/gpt-main");
    drop(hub);
}

#[tokio::test]
async fn unprefixed_model_is_rejected_with_profile_list() {
    let upstream = MockServer::start().await;
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("gpt-4o"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("solo"));
    drop(hub);
}

const AUDIO_BOUNDARY: &str = "VoiceNotesBoundary7";

fn transcription_form(model: &str) -> Vec<u8> {
    format!(
        "--{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n\
         --{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.m4a\"\r\n\
         Content-Type: audio/mp4\r\n\r\nAUDIO-BYTES\r\n--{AUDIO_BOUNDARY}--\r\n"
    )
    .into_bytes()
}

async fn post_transcription(base: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={AUDIO_BOUNDARY}"),
        )
        .header("x-llm-hub-fallbacks", "healthy/whisper-fallback")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn multipart_transcription_routes_by_model_field_and_fails_over() {
    let limited = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(body_string_contains("\r\n\r\nwhisper-main\r\n"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&limited)
        .await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(body_string_contains("\r\n\r\nwhisper-fallback\r\n"))
        .and(body_string_contains("AUDIO-BYTES"))
        .and(header(
            "content-type",
            format!("multipart/form-data; boundary={AUDIO_BOUNDARY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hi"})))
        .expect(1)
        .mount(&healthy)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "limited,healthy".into()),
        ("LLM_HUB_LIMITED_BASE_URL".into(), limited.uri()),
        ("LLM_HUB_HEALTHY_BASE_URL".into(), healthy.uri()),
    ]))
    .await;

    let response = post_transcription(&base, transcription_form("limited/whisper-main")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-model"],
        "healthy/whisper-fallback"
    );
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["text"],
        "hi"
    );
    drop(hub);
}

#[tokio::test]
async fn multipart_without_model_field_is_rejected() {
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "healthy".into()),
        (
            "LLM_HUB_HEALTHY_BASE_URL".into(),
            "http://127.0.0.1:9".into(),
        ),
    ]))
    .await;

    let without_model = String::from_utf8(transcription_form("x"))
        .unwrap()
        .replace("name=\"model\"", "name=\"prompt\"");
    let response = post_transcription(&base, without_model.into_bytes()).await;

    assert_eq!(response.status(), 400);
    drop(hub);
}

#[tokio::test]
async fn multipart_over_upload_limit_is_rejected() {
    let (hub, base) = start_hub(HashMap::from([
        (
            "LLM_HUB_MAX_REPLAY_BYTES".into(),
            (2 * 1024 * 1024).to_string(),
        ),
        ("LLM_HUB_PROFILES".into(), "healthy".into()),
        (
            "LLM_HUB_HEALTHY_BASE_URL".into(),
            "http://127.0.0.1:9".into(),
        ),
    ]))
    .await;

    let oversized_audio = "A".repeat(26 * 1024 * 1024);
    let body = String::from_utf8(transcription_form("healthy/whisper"))
        .unwrap()
        .replace("AUDIO-BYTES", &oversized_audio);
    let response = post_transcription(&base, body.into_bytes()).await;

    assert_eq!(response.status(), 400);
    drop(hub);
}

const SMART_AUTO_EXCLUDE: &str = "x-llm-hub-smart-auto-exclude";
const SMART_AUTO_WORKER: &str = "x-llm-hub-smart-auto-worker";

/// An upstream whose `/models` lists one chat model.
async fn listing_upstream(model: &str) -> MockServer {
    listing_upstream_entry(serde_json::json!({"id": model})).await
}

/// An upstream whose `/models` lists this one entry.
async fn listing_upstream_entry(entry: serde_json::Value) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": "list", "data": [entry]})),
        )
        .mount(&upstream)
        .await;
    upstream
}

async fn chat_requests(upstream: &MockServer) -> Vec<wiremock::Request> {
    upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect()
}

async fn error_message(response: reqwest::Response) -> String {
    let body: serde_json::Value = response.json().await.unwrap();
    body["error"]["message"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn hub_selector_resolves_to_a_concrete_eligible_model() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&alpha)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_WORKER, "run-1/node-1")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "alpha/m1");
    let sent = chat_requests(&alpha).await;
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["model"], "m1");
    assert!(
        sent[0]
            .headers
            .keys()
            .all(|name| !name.as_str().starts_with("x-llm-hub-smart-auto-")),
        "Smart Auto headers reached the upstream: {:?}",
        sent[0].headers
    );
    drop(hub);
}

#[tokio::test]
async fn hub_selector_with_every_model_excluded_is_rejected_before_any_upstream_call() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_EXCLUDE, "alpha/*,beta/*")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let message = error_message(response).await;
    assert!(
        message.contains("smart_auto_no_eligible_model"),
        "{message}"
    );
    assert!(message.contains("exclude alpha/*|beta/*"), "{message}");
    assert!(chat_requests(&alpha).await.is_empty());
    assert!(chat_requests(&beta).await.is_empty());
    drop(hub);
}

#[tokio::test]
async fn hub_fallback_excluded_by_header_is_never_tried() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&alpha)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(0)
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-fallbacks", "beta/m2")
        .header(SMART_AUTO_EXCLUDE, "beta/*")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["x-llm-hub-attempts"], "alpha/m1=429");
    drop(hub);
}

#[tokio::test]
async fn direct_model_ignores_smart_auto_headers() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_EXCLUDE, "beta/*")
        .header("x-llm-hub-smart-auto-tier", "not-a-tier")
        .json(&chat_body("beta/m2"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "beta/m2");
    let sent = chat_requests(&beta).await;
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["model"], "m2");
    assert!(
        sent[0]
            .headers
            .keys()
            .all(|name| !name.as_str().starts_with("x-llm-hub-smart-auto-")),
        "Smart Auto headers reached the upstream: {:?}",
        sent[0].headers
    );
    assert!(chat_requests(&alpha).await.is_empty());
    drop(hub);
}

#[tokio::test]
async fn unknown_hub_model_is_rejected() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("hub/foo"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let message = error_message(response).await;
    assert!(message.contains("unknown hub model hub/foo"), "{message}");
    assert!(chat_requests(&alpha).await.is_empty());
    assert!(chat_requests(&beta).await.is_empty());
    drop(hub);
}

#[tokio::test]
async fn hub_selector_picks_only_from_the_profiles_the_caller_allows() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-profiles", "hub,beta")
        .header("x-llm-hub-fallbacks", "alpha/m1")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "beta/m2");
    assert_eq!(chat_requests(&beta).await.len(), 1);
    assert!(chat_requests(&alpha).await.is_empty());
    drop(hub);
}

#[tokio::test]
async fn hub_selector_skips_a_model_whose_input_limit_is_below_the_prompt() {
    let alpha =
        listing_upstream_entry(serde_json::json!({"id": "m1", "max_input_tokens": 1000})).await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let mut body = chat_body("hub/smart-auto-balance");
    body["messages"][0]["content"] = serde_json::Value::String("x".repeat(8_000));

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "beta/m2");
    assert!(chat_requests(&alpha).await.is_empty());
    drop(hub);
}

#[tokio::test]
async fn smart_auto_sends_the_effort_the_routed_model_publishes() {
    let alpha = listing_upstream_entry(serde_json::json!({
        "id": "m1",
        "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
    }))
    .await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&alpha)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_EXCLUDE, "beta/*")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "alpha/m1");
    assert_eq!(response.headers()["x-llm-hub-smart-auto-tier"], "balance");
    assert_eq!(response.headers()["x-llm-hub-smart-auto-effort"], "medium");
    let sent = chat_requests(&alpha).await;
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["reasoning_effort"], "medium");
    drop(hub);
}

#[tokio::test]
async fn a_tool_continuation_stays_on_the_routed_solver() {
    const CLAUDE_SESSION: &str = "x-claude-code-session-id";
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    for upstream in [&alpha, &beta] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_completion())
            .mount(upstream)
            .await;
    }
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base}/v1/chat/completions"))
        .header(CLAUDE_SESSION, "session-1")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let solver = first.headers()["x-llm-hub-model"]
        .to_str()
        .unwrap()
        .to_owned();
    // Reading the body completes the stream, and the stats task then commits the pin.
    first.bytes().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let continuation = serde_json::json!({
        "model": "hub/smart-auto-balance",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "read", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call-1", "content": "file body"}
        ]
    });
    let second = client
        .post(format!("{base}/v1/chat/completions"))
        .header(CLAUDE_SESSION, "session-1")
        .json(&continuation)
        .send()
        .await
        .unwrap();

    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-llm-hub-model"], solver.as_str());
    assert_eq!(
        second.headers()["x-llm-hub-smart-auto-reason"],
        "tool_continuation_replay"
    );
    drop(hub);
}

#[tokio::test]
async fn a_tier_header_overrides_the_tier_of_the_model() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    for upstream in [&alpha, &beta] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_completion())
            .mount(upstream)
            .await;
    }
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-smart-auto-tier", "fast")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();

    let decisions: serde_json::Value = client
        .get(format!("{base}/api/smart-auto/decisions?limit=1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(decisions["decisions"][0]["tier"], "fast", "{decisions}");
}

#[tokio::test]
async fn a_tier_header_that_matches_the_model_is_accepted() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&alpha)
        .await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-smart-auto-tier", "balance")
        .json(&chat_body("hub/smart-auto-balance"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn smart_auto_dispatch_is_visible_in_the_selection_history() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&alpha)
        .await;
    // Balance reviews planning and implementation turns with one judge on the other profile, so beta accepts it.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(wiremock::matchers::body_string_contains("<draft>"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-judge", "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"verdict\":\"accept\"}"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        })))
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();
    let worker = "run-history/node-1";

    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_WORKER, worker)
        .json(&chat_body_with(
            "hub/smart-auto-balance",
            "Plan the migration of the billing service",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();

    let history_url = format!("{base}/api/smart-auto/history?worker={worker}");
    let mut states = Vec::new();
    let mut selected = serde_json::Value::Null;
    for _ in 0..100 {
        let history: serde_json::Value = client
            .get(&history_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        states = history["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| {
                (
                    event["state"].as_str().unwrap().to_string(),
                    event["model"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        if states.len() == 5 {
            selected = history["events"][0].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let expected = [
        ("selected", "alpha/m1"),
        ("started", "alpha/m1"),
        ("started", "beta/m2"),
        ("completed", "beta/m2"),
        ("completed", "alpha/m1"),
    ];
    let expected: Vec<(String, String)> = expected
        .iter()
        .map(|(state, model)| (state.to_string(), model.to_string()))
        .collect();
    assert_eq!(states, expected);
    assert_eq!(selected["pin_miss"], "no_pin");
    assert_eq!(selected["affinity_scope"], "content_hash");
    drop(hub);
}

#[tokio::test]
async fn resolve_names_the_model_without_calling_an_upstream() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/smart-auto/resolve");

    let reply: serde_json::Value = client
        .post(&url)
        .json(&serde_json::json!({"tier": "intelligence", "role": "planning"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let model = reply["model"].as_str().unwrap();
    assert!(["alpha/m1", "beta/m2"].contains(&model), "{reply}");
    assert_eq!(reply["tier"], "intelligence");
    assert_eq!(reply["role"], "planning");
    assert_eq!(reply["pinned"], false);
    assert_eq!(reply["reason"], "fresh");
    let alternatives = reply["alternatives"].as_array().unwrap();
    assert_eq!(alternatives.len(), 1, "{reply}");
    assert_ne!(alternatives[0], reply["model"]);
    assert!(chat_requests(&alpha).await.is_empty());
    assert!(chat_requests(&beta).await.is_empty());

    let only_beta: serde_json::Value = client
        .post(&url)
        .json(&serde_json::json!({"include": ["beta/*"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(only_beta["model"], "beta/m2");
    assert_eq!(only_beta["tier"], "balance");

    for (body, status) in [
        (serde_json::json!({"tier": "max"}), 400),
        (serde_json::json!({"role": "coding"}), 400),
        (serde_json::json!({"include": ["nobody/*"]}), 409),
    ] {
        let response = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{body}");
    }
}

#[tokio::test]
async fn tiers_lists_each_tier_pool_and_the_role_defaults() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let reply: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/smart-auto/tiers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(reply["default_tier"], "balance");
    let tiers = reply["tiers"].as_array().unwrap();
    assert_eq!(tiers.len(), 4);
    assert_eq!(tiers[2]["name"], "intelligence");
    assert_eq!(tiers[2]["selector"], "hub/smart-auto-intelligence");
    assert_eq!(tiers[2]["pool"], serde_json::json!(["alpha/m1", "beta/m2"]));
    assert_eq!(reply["roles"].as_array().unwrap().len(), 5);
    assert_eq!(reply["role_tier_defaults"]["planning"], "intelligence");
    assert_eq!(reply["role_tier_defaults"]["conversation"], "fast");
}

const SESSION_ID: &str = "x-llm-hub-session-id";

/// Sends one `hub/smart-auto-fast` turn on `route`, then polls resolve until it reports the pin.
async fn resolve_after_a_turn(route: &str, body: serde_json::Value) {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    for upstream in [&alpha, &beta] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_completion())
            .mount(upstream)
            .await;
    }
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();
    let worker = format!("run-pin{route}");
    let session = "pin-session-1";

    let response = client
        .post(format!("{base}{route}"))
        .header(SMART_AUTO_WORKER, &worker)
        .header(SESSION_ID, session)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let served = response.headers()["x-llm-hub-model"]
        .to_str()
        .unwrap()
        .to_string();
    response.bytes().await.unwrap();

    let url = format!("{base}/api/smart-auto/resolve");
    let query = serde_json::json!({"tier": "fast", "worker": worker, "session": session});
    let mut reply = serde_json::Value::Null;
    for _ in 0..100 {
        reply = client
            .post(&url)
            .json(&query)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if reply["pinned"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reply["pinned"], true, "{reply}");
    assert_eq!(reply["model"], served.as_str(), "{reply}");

    // The profiles filter leaves the pin out of the pool, and the dry run does not drop it.
    let other = if served.starts_with("alpha/") {
        "beta"
    } else {
        "alpha"
    };
    let mut filtered = query.clone();
    filtered["profiles"] = serde_json::json!(other);
    let reply: serde_json::Value = client
        .post(&url)
        .json(&filtered)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reply["pinned"], false, "{reply}");
    assert!(
        reply["model"].as_str().unwrap().starts_with(other),
        "{reply}"
    );
    let again: serde_json::Value = client
        .post(&url)
        .json(&query)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again["pinned"], true, "{again}");
}

#[tokio::test]
async fn resolve_reports_a_pin_set_through_messages() {
    resolve_after_a_turn(
        "/v1/messages",
        serde_json::json!({"model": "hub/smart-auto-fast", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
}

#[tokio::test]
async fn resolve_reports_a_pin_set_through_chat_completions() {
    resolve_after_a_turn("/v1/chat/completions", chat_body("hub/smart-auto-fast")).await;
}

#[tokio::test]
async fn resolve_finds_a_pin_set_with_a_parent() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    for upstream in [&alpha, &beta] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ok_completion())
            .mount(upstream)
            .await;
    }
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{base}/v1/chat/completions"))
        .header(SMART_AUTO_WORKER, "run-parent")
        .header("x-llm-hub-smart-auto-parent", "flow-1")
        .header(SESSION_ID, "parent-session")
        .json(&chat_body("hub/smart-auto-fast"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let served = response.headers()["x-llm-hub-model"]
        .to_str()
        .unwrap()
        .to_string();
    response.bytes().await.unwrap();

    let query = serde_json::json!({"tier": "fast", "worker": "run-parent",
        "session": "parent-session", "parent": "flow-1"});
    let mut reply = serde_json::Value::Null;
    for _ in 0..100 {
        reply = client
            .post(format!("{base}/api/smart-auto/resolve"))
            .json(&query)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if reply["pinned"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(reply["pinned"], true, "{reply}");
    assert_eq!(reply["model"], served.as_str(), "{reply}");
}

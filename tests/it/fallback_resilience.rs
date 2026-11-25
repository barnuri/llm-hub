//! Fallback resilience: a broken success, a model that is not ready and an out-of-credits reply
//! move to the next fallback; rate limits cool for the reset the upstream names; one deadline
//! bounds the whole chain.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

use std::collections::HashMap;
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::hub;

use hub::{InProcessHub, start_hub};

const FALLBACKS: &str = "x-llm-hub-fallbacks";

fn chat_body(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({"model": model, "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]})
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }))
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/event-stream")
}

async fn upstream(reply: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(reply)
        .mount(&server)
        .await;
    server
}

async fn chat_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .count()
}

/// A hub with the profiles `first` and `second`, plus `extra` env pairs.
async fn two_profile_hub(
    first: &MockServer,
    second: &MockServer,
    extra: &[(&str, &str)],
) -> (InProcessHub, String) {
    let mut vars = HashMap::from([
        ("LLM_HUB_PROFILES".to_owned(), "first,second".to_owned()),
        ("LLM_HUB_FIRST_BASE_URL".to_owned(), first.uri()),
        ("LLM_HUB_SECOND_BASE_URL".to_owned(), second.uri()),
    ]);
    for (key, value) in extra {
        vars.insert((*key).to_owned(), (*value).to_owned());
    }
    start_hub(vars).await
}

async fn send(base: &str, body: &serde_json::Value, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(FALLBACKS, "second/backup");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.json(body).send().await.unwrap()
}

fn attempts(response: &reqwest::Response) -> String {
    response.headers()["x-llm-hub-attempts"]
        .to_str()
        .unwrap()
        .to_owned()
}

async fn health(base: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{base}/api/smart-auto/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_empty_stream_moves_to_the_next_fallback() {
    let empty = upstream(sse("")).await;
    let healthy = upstream(sse("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n")).await;
    let (_hub, base) = two_profile_hub(&empty, &healthy, &[]).await;

    let response = send(&base, &chat_body("first/main", true), &[]).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");
    let trail = attempts(&response);
    assert!(trail.contains("first/main=empty reply"), "{trail}");
    assert!(response.text().await.unwrap().contains("\"ok\""));
}

#[tokio::test]
async fn an_empty_json_success_moves_to_the_next_fallback() {
    let empty = upstream(ResponseTemplate::new(200)).await;
    let healthy = upstream(ok_completion()).await;
    let (_hub, base) = two_profile_hub(&empty, &healthy, &[]).await;

    let response = send(&base, &chat_body("first/main", false), &[]).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");
}

#[tokio::test]
async fn an_opening_error_event_moves_to_the_next_fallback() {
    let broken = upstream(sse(
        ": hello\n\ndata: {\"error\":{\"message\":\"backend crashed\",\"type\":\"server_error\"}}\n\n",
    ))
    .await;
    let healthy = upstream(sse("data: {\"choices\":[]}\n\ndata: [DONE]\n\n")).await;
    let (_hub, base) = two_profile_hub(&broken, &healthy, &[]).await;

    let response = send(&base, &chat_body("first/main", true), &[]).await;

    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");
    let trail = attempts(&response);
    assert!(trail.contains("error event: backend crashed"), "{trail}");
    let row = health(&base).await["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == "first/main")
        .cloned()
        .unwrap();
    assert_eq!(row["last_failure"], "broken_reply");
}

#[tokio::test]
async fn without_a_fallback_an_empty_stream_reaches_the_client_as_before() {
    let empty = upstream(sse("")).await;
    let (_hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "first".into()),
        ("LLM_HUB_FIRST_BASE_URL".into(), empty.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("first/main", true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(response.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_model_that_is_not_ready_moves_on_and_a_genuine_bad_request_does_not() {
    let loading = upstream(ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {"code": 400, "message": "Failed to initialize samplers", "type": "invalid_request_error"}
    })))
    .await;
    let healthy = upstream(ok_completion()).await;
    let (_hub, base) = two_profile_hub(&loading, &healthy, &[]).await;

    let response = send(&base, &chat_body("first/main", false), &[]).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");
    assert!(attempts(&response).contains("first/main=400"));

    let rejecting = upstream(ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {"message": "messages: field required", "type": "invalid_request_error"}
    })))
    .await;
    let never = upstream(ok_completion()).await;
    let (_hub2, base2) = two_profile_hub(&rejecting, &never, &[]).await;
    let response = send(&base2, &chat_body("first/main", false), &[]).await;
    assert_eq!(response.status(), 400);
    assert_eq!(chat_count(&never).await, 0);
}

#[tokio::test]
async fn a_rate_limit_cools_the_model_until_its_reset() {
    let limited = upstream(ResponseTemplate::new(429).insert_header("retry-after", "120")).await;
    let healthy = upstream(ok_completion()).await;
    let (_hub, base) = two_profile_hub(&limited, &healthy, &[]).await;

    let response = send(&base, &chat_body("first/main", false), &[]).await;
    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");

    let report = health(&base).await;
    let row = report["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == "first/main")
        .cloned()
        .unwrap();
    let cooled = row["cooled_for_ms"].as_u64().unwrap();
    assert!((100_000..=120_000).contains(&cooled), "{row}");
    assert!(
        row["cooled_reason"]
            .as_str()
            .unwrap()
            .contains("retry-after"),
        "{row}"
    );
    assert!(row["cooled_until_ms"].as_u64().is_some(), "{row}");
}

#[tokio::test]
async fn out_of_credits_moves_on_and_cools_the_profile() {
    let broke = upstream(ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": "Your credit balance is too low to access the API."}
    })))
    .await;
    let healthy = upstream(ok_completion()).await;
    let (_hub, base) =
        two_profile_hub(&broke, &healthy, &[("LLM_HUB_QUOTA_COOLDOWN_SECS", "600")]).await;

    let response = send(&base, &chat_body("first/main", false), &[]).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "second/backup");
    let report = health(&base).await;
    let profiles = report["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 1, "{report}");
    assert_eq!(profiles[0]["profile"], "first");
    let cooled = profiles[0]["cooled_for_ms"].as_u64().unwrap();
    assert!((500_000..=600_000).contains(&cooled), "{report}");
    assert!(
        profiles[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("out of credits"),
        "{report}"
    );
}

#[tokio::test]
async fn the_deadline_skips_the_remaining_fallbacks_with_a_504() {
    let failing = upstream(ResponseTemplate::new(503).set_delay(Duration::from_millis(400))).await;
    let never = upstream(ok_completion()).await;
    let (_hub, base) = two_profile_hub(&failing, &never, &[]).await;

    let response = send(
        &base,
        &chat_body("first/main", false),
        &[("x-llm-hub-deadline-ms", "300")],
    )
    .await;

    assert_eq!(response.status(), 504);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("deadline of 300 ms"), "{message}");
    assert!(message.contains("first/main=timeout"), "{message}");
    assert!(message.contains("skipped: second/backup"), "{message}");
    assert_eq!(chat_count(&never).await, 0);
}

#[tokio::test]
async fn the_configured_deadline_cuts_a_slow_last_attempt() {
    let failing = upstream(ResponseTemplate::new(500)).await;
    // The bound below is far under the upstream delay, so a loaded machine cannot fake a pass or a failure.
    let slow = upstream(ok_completion().set_delay(Duration::from_secs(2))).await;
    let (_hub, base) = two_profile_hub(&failing, &slow, &[("LLM_HUB_DEADLINE_MS", "500")]).await;

    let started = std::time::Instant::now();
    let response = send(&base, &chat_body("first/main", false), &[]).await;

    assert_eq!(response.status(), 504);
    assert!(started.elapsed() < Duration::from_millis(1900));

    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("first/main=500"), "{message}");
    assert!(message.contains("second/backup=timeout"), "{message}");

    let response = send(
        &base,
        &chat_body("first/main", false),
        &[("x-llm-hub-deadline-ms", "0")],
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "0 in the header turns the limit off"
    );
}

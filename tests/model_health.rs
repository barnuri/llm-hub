//! Smart Auto marks a model that fails or stays silent, and leaves it out of the next pick.

use std::collections::HashMap;
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "support/hub.rs"]
mod hub;

use hub::{start_alpha_beta_hub, start_alpha_beta_hub_with};

const BALANCE: &str = "hub/smart-auto-balance";
const FALLBACKS: &str = "x-llm-hub-fallbacks";

fn chat_body(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": BALANCE,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }))
}

async fn listing_upstream(model: &str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": "list", "data": [{"id": model}]})),
        )
        .mount(&upstream)
        .await;
    upstream
}

async fn chat_count(upstream: &MockServer) -> usize {
    upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .count()
}

async fn health_row(base: &str, model: &str) -> serde_json::Value {
    let health: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/api/smart-auto/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    health["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == model)
        .unwrap_or_else(|| panic!("no health row for {model}: {health}"))
        .clone()
}

#[tokio::test]
async fn a_failing_model_sits_out_of_the_next_pick() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&alpha)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();

    let first = client
        .post(format!("{base}/v1/chat/completions"))
        .header(FALLBACKS, "beta/m2")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["x-llm-hub-model"], "beta/m2");
    assert!(
        first.headers()["x-llm-hub-attempts"]
            .to_str()
            .unwrap()
            .starts_with("alpha/m1=503"),
    );

    let second = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(second.headers()["x-llm-hub-model"], "beta/m2");
    assert_eq!(second.headers()["x-llm-hub-attempts"], "beta/m2=200");
    assert_eq!(chat_count(&alpha).await, 1, "alpha was picked again");

    let row = health_row(&base, "alpha/m1").await;
    assert_eq!(row["failures"], 1);
    assert!(row["cooled_for_ms"].as_u64().unwrap() > 0, "{row}");
    drop(hub);
}

#[tokio::test]
async fn a_silent_streamed_model_times_out_and_is_marked() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion().set_delay(Duration::from_secs(5)))
        .mount(&alpha)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub_with(
        &alpha,
        &beta,
        HashMap::from([("LLM_HUB_ALPHA_FIRST_BYTE_TIMEOUT_MS".into(), "300".into())]),
    )
    .await;

    let started = std::time::Instant::now();
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(FALLBACKS, "beta/m2")
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "beta/m2");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "waited {:?} for a silent model",
        started.elapsed()
    );
    let row = health_row(&base, "alpha/m1").await;
    assert_eq!(row["last_failure"], "timeout");
    assert!(row["cooled_for_ms"].as_u64().unwrap() > 0, "{row}");
    drop(hub);
}

#[tokio::test]
async fn a_buffered_request_is_not_cut_short() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion().set_delay(Duration::from_millis(1_200)))
        .mount(&alpha)
        .await;
    let (hub, base) = start_alpha_beta_hub_with(
        &alpha,
        &beta,
        HashMap::from([("LLM_HUB_ALPHA_FIRST_BYTE_TIMEOUT_MS".into(), "300".into())]),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "alpha/m1");
    drop(hub);
}

#[tokio::test]
async fn a_request_error_does_not_mark_the_model() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&alpha)
        .await;
    let (hub, base) = start_alpha_beta_hub(&alpha, &beta).await;
    let client = reqwest::Client::new();

    for _ in 0..2 {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .json(&chat_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(response.headers()["x-llm-hub-model"], "alpha/m1");
    }
    assert_eq!(chat_count(&alpha).await, 2);
    drop(hub);
}

#[tokio::test]
async fn a_hung_pick_is_answered_by_the_next_best_model_in_the_same_request() {
    let alpha = listing_upstream("m1").await;
    let beta = listing_upstream("m2").await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion().set_delay(Duration::from_secs(5)))
        .mount(&alpha)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .mount(&beta)
        .await;
    let (hub, base) = start_alpha_beta_hub_with(
        &alpha,
        &beta,
        HashMap::from([("LLM_HUB_ALPHA_FIRST_BYTE_TIMEOUT_MS".into(), "300".into())]),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "beta/m2");
    let attempts = response.headers()["x-llm-hub-attempts"].to_str().unwrap();
    assert!(attempts.starts_with("alpha/m1=timeout"), "{attempts}");
    assert!(attempts.ends_with("beta/m2=200"), "{attempts}");
    drop(hub);
}

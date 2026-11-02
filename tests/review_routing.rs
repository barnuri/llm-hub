//! Review routing: Efficiency streams one call unreviewed, Balance reviews the final answer, and a Responses caller is never reviewed.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[path = "support/hub.rs"]
mod hub;

use hub::{BodyHas, BodyLacks, completion, start_alpha_beta_hub};

/// The marker only a review judge request carries: its user text wraps the draft in `<draft>`.
const JUDGE_MARKER: &str = "<draft>";

/// An upstream whose `/models` lists one chat model, and whose chat route answers as a solver
/// (`draft`) or as an accepting judge, depending on the request it receives.
async fn review_upstream(model: &str, draft: &str) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"object": "list", "data": [{"id": model}]})),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(BodyLacks(JUDGE_MARKER))
        .respond_with(completion(draft))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(BodyHas(JUDGE_MARKER))
        .respond_with(completion(r#"{"verdict":"accept"}"#))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_1", "object": "response", "output": []
        })))
        .mount(&upstream)
        .await;
    upstream
}

fn chat_body(model: &str, stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "stream": stream,
        "messages": [{"role": "user", "content": "Plan how to sum 2 and 3 across the two services."}]
    })
}

/// Every request the upstream received on `route` and whether it carried the judge marker.
async fn requests_on(upstream: &MockServer, route: &str, judge: bool) -> Vec<Request> {
    upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == route)
        .filter(|request| String::from_utf8_lossy(&request.body).contains(JUDGE_MARKER) == judge)
        .collect()
}

#[tokio::test]
async fn efficiency_tier_streams_one_call_and_never_reviews() {
    let alpha = review_upstream("m1", "The sum is 5.").await;
    let beta = review_upstream("m2", "The sum is 5.").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("hub/smart-auto-efficiency", true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let served = response.headers()["x-llm-hub-model"]
        .to_str()
        .unwrap()
        .to_owned();
    let solver = if served == "alpha/m1" { &alpha } else { &beta };
    let other = if served == "alpha/m1" { &beta } else { &alpha };
    let sent = requests_on(solver, "/chat/completions", false).await;
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert!(
        requests_on(other, "/chat/completions", true)
            .await
            .is_empty()
    );
    assert!(
        requests_on(solver, "/chat/completions", true)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn balance_tier_reviews_with_one_tool_free_judge() {
    let alpha = review_upstream("m1", "The sum is 5.").await;
    let beta = review_upstream("m2", "The sum is 5.").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("hub/smart-auto-balance", false))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "The sum is 5.");
    let judges = [&alpha, &beta];
    let mut judge_calls = Vec::new();
    for upstream in judges {
        judge_calls.extend(requests_on(upstream, "/chat/completions", true).await);
    }
    assert_eq!(judge_calls.len(), 1);
    let judge: serde_json::Value = serde_json::from_slice(&judge_calls[0].body).unwrap();
    assert!(judge.get("tools").is_none());
    assert_eq!(judge["stream"], false);
}

#[tokio::test]
async fn balance_tier_streams_the_reviewed_answer_as_sse_for_a_streaming_client() {
    let alpha = review_upstream("m1", "The sum is 5.").await;
    let beta = review_upstream("m2", "The sum is 5.").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("hub/smart-auto-balance", true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = response.text().await.unwrap();
    assert!(text.contains("The sum is 5."));
    assert!(text.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn responses_route_skips_review() {
    let alpha = review_upstream("m1", "The sum is 5.").await;
    let beta = review_upstream("m2", "The sum is 5.").await;
    let (_hub, base) = start_alpha_beta_hub(&alpha, &beta).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .json(&serde_json::json!({"model": "hub/smart-auto-balance", "input": "Sum 2 and 3."}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    for upstream in [&alpha, &beta] {
        assert!(
            requests_on(upstream, "/chat/completions", true)
                .await
                .is_empty()
        );
    }
}

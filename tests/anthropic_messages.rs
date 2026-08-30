//! `/v1/messages`: Anthropic in, OpenAI upstream, Anthropic out — plus the
//! control-header interop the route inherits from the shared fallback loop.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use wiremock::matchers::{body_partial_json, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_hub(vars: HashMap<String, String>) -> (HubProcess, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = env!("CARGO_BIN_EXE_llm-hub");
    let dir = tempfile::tempdir().unwrap().keep();
    let mut command = Command::new(binary);
    command
        .current_dir(&dir)
        .env("LLM_HUB_PORT", port.to_string())
        .env("LLM_HUB_AUTO_UPDATE", "0");
    for (key, value) in vars {
        command.env(key, value);
    }
    let child = command.spawn().expect("hub starts");
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    for _ in 0..50 {
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (HubProcess(child), base)
}

fn messages_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "max_tokens": 64,
        "system": "be terse",
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-7", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 6, "total_tokens": 11}
    }))
}

/// An upstream that emits tool calls but still reports `finish_reason: "stop"`
/// (llama.cpp, vLLM in some configs, several gateways). The translated message
/// must still say `tool_use`, or an Anthropic client ends its agent loop
/// without ever running the tool.
fn tool_call_completion_with_stop_finish_reason() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-9", "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant", "content": serde_json::Value::Null,
            "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "lookup", "arguments": "{\"q\":\"rust\"}"}}]
        }}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 6, "total_tokens": 11}
    }))
}

#[tokio::test]
async fn messages_tool_calls_with_stop_finish_reason_report_tool_use() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(tool_call_completion_with_stop_finish_reason())
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body("solo/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["content"][0]["type"], "tool_use");
    // The id round-trips verbatim so the client can echo it as tool_use_id.
    assert_eq!(body["content"][0]["id"], "call_1");
    assert_eq!(body["content"][0]["name"], "lookup");
    assert_eq!(
        body["content"][0]["input"],
        serde_json::json!({"q": "rust"})
    );
    drop(hub);
}

#[tokio::test]
async fn messages_non_stream_round_trip() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // The Anthropic `system` field arrives upstream as a system message.
        .and(body_partial_json(serde_json::json!({
            "model": "gpt-main",
            "messages": [{"role": "system", "content": "be terse"}]
        })))
        .respond_with(ok_completion())
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body("solo/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "solo/gpt-main");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["id"], "msg_chatcmpl-7");
    assert_eq!(body["model"], "solo/gpt-main");
    assert_eq!(
        body["content"],
        serde_json::json!([{"type": "text", "text": "ok"}])
    );
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["stop_sequence"], serde_json::Value::Null);
    assert_eq!(
        body["usage"],
        serde_json::json!({"input_tokens": 5, "output_tokens": 6})
    );
    drop(hub);
}

#[tokio::test]
async fn messages_honours_fallback_chain_and_sets_attempts_header() {
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
        .post(format!("{base}/v1/messages"))
        .header("x-llm-hub-fallbacks", "healthy/gpt-fallback")
        .json(&messages_body("limited/gpt-main"))
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
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["model"], "healthy/gpt-fallback");
    drop(hub);
}

#[tokio::test]
async fn messages_upstream_400_returns_anthropic_error_shape_and_status() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": {"message": "bad model"}})),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body("solo/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["message"], "bad model");
    drop(hub);
}

#[tokio::test]
async fn messages_unprefixed_model_is_400_listing_profiles() {
    let upstream = MockServer::start().await;
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body("claude-sonnet-4"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("solo"),
        "message: {}",
        body["error"]["message"]
    );
    drop(hub);
}

#[tokio::test]
async fn messages_oversized_body_is_400_not_passthrough() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(0)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
        ("LLM_HUB_MAX_REPLAY_BYTES".into(), "128".into()),
    ]))
    .await;

    let mut body = messages_body("solo/gpt-main");
    body["messages"][0]["content"] = serde_json::Value::String("x".repeat(4096));

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MAX_REPLAY_BYTES")
    );
    drop(hub);
}

/// A representative OpenAI stream: role chunk, two text deltas, a
/// `finish_reason` chunk, the terminal usage chunk, then `[DONE]`.
fn sse_completion() -> ResponseTemplate {
    let body = concat!(
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"}}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-9\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6}}\n\n",
        "data: [DONE]\n\n",
    );
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// `(event name, payload)` pairs, in emission order.
fn parse_sse(text: &str) -> Vec<(String, serde_json::Value)> {
    text.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    name = rest.to_string();
                }
                if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            (name, serde_json::from_str(&data).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn messages_stream_round_trip_emits_anthropic_events() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // The hub asks for the terminal usage chunk so token stats survive.
        .and(body_partial_json(serde_json::json!({
            "stream": true,
            "stream_options": {"include_usage": true}
        })))
        .respond_with(sse_completion())
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let mut body = messages_body("solo/gpt-main");
    body["stream"] = serde_json::Value::Bool(true);
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "solo/gpt-main");
    let events = parse_sse(&response.text().await.unwrap());
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "events: {events:?}"
    );
    assert_eq!(events[0].1["message"]["id"], "msg_chatcmpl-9");
    assert_eq!(events[0].1["message"]["model"], "solo/gpt-main");
    assert_eq!(events[2].1["delta"]["text"], "he");
    assert_eq!(events[3].1["delta"]["text"], "llo");
    assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
    assert_eq!(
        events[5].1["usage"],
        serde_json::json!({"input_tokens": 5, "output_tokens": 6})
    );
    drop(hub);
}

#[tokio::test]
async fn messages_stream_sets_text_event_stream_content_type() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_completion())
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let mut body = messages_body("solo/gpt-main");
    body["stream"] = serde_json::Value::Bool(true);
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "content-type: {:?}",
        response.headers()["content-type"]
    );
    assert!(response.headers().get("content-encoding").is_none());
    drop(hub);
}

/// A streaming request whose upstream answers with a JSON error must be
/// translated as a message, not fed to the SSE machinery.
#[tokio::test]
async fn stream_request_with_json_error_response_is_translated_as_an_error() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(serde_json::json!({"error": {"message": "slow down"}})),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let mut body = messages_body("solo/gpt-main");
    body["stream"] = serde_json::Value::Bool(true);
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-llm-hub-retry-on", "")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    drop(hub);
}

#[tokio::test]
async fn anthropic_version_header_is_not_forwarded_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header_exists("anthropic-version"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(1)
        .mount(&upstream)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "tools-2024-04-04")
        .json(&messages_body("solo/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    drop(hub);
}

// --- tool-name truncation on the Anthropic route ---

/// 77 bytes: longer than the 64 an OpenAI-compatible upstream will accept.
const LONG_TOOL: &str =
    "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";

/// Answers with a call to whatever tool it was handed. Nothing here knows how
/// the alias is derived — only that the Anthropic client must never see it.
struct EchoToolName {
    stream: bool,
}

impl Respond for EchoToolName {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("upstream body is json");
        let name = body["tools"][0]["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(name.len(), 64, "aliased to the provider's limit: {name}");
        assert_ne!(name, LONG_TOOL);
        if self.stream {
            let sse = format!(
                concat!(
                    "data: {{\"id\":\"chatcmpl-9\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"function\":{{\"name\":\"{name}\",\"arguments\":\"\"}}}}]}}}}]}}\n\n",
                    "data: {{\"id\":\"chatcmpl-9\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
                name = name
            );
            return ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-7",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": serde_json::Value::Null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": name, "arguments": "{}"}}],
            }}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 6}
        }))
    }
}

/// An Anthropic request declaring one over-long tool.
fn messages_with_tool(stream: bool) -> serde_json::Value {
    let mut body = messages_body("solo/gpt-main");
    body["stream"] = serde_json::Value::Bool(stream);
    body["tools"] = serde_json::json!([{
        "name": LONG_TOOL, "description": "d",
        "input_schema": {"type": "object", "properties": {}},
    }]);
    body
}

/// The alias the hub actually sent upstream, read back off the recording.
async fn sent_alias(upstream: &MockServer) -> String {
    upstream
        .received_requests()
        .await
        .expect("recording enabled")
        .into_iter()
        .find(|request| request.url.path() == "/chat/completions")
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["tools"][0]["function"]["name"]
                .as_str()
                .expect("tool name is a string")
                .to_string()
        })
        .expect("one upstream attempt")
}

async fn hub_with_echo(upstream: &MockServer, stream: bool) -> (HubProcess, String) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(EchoToolName { stream })
        .expect(1)
        .mount(upstream)
        .await;
    start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await
}

#[tokio::test]
async fn messages_restores_tool_use_name_in_translated_output() {
    let upstream = MockServer::start().await;
    let (hub, base) = hub_with_echo(&upstream, false).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_with_tool(false))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-transforms"], "tool-names=1");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["name"], LONG_TOOL);
    assert_eq!(body["content"][0]["input"], serde_json::json!({}));
    assert_eq!(body["stop_reason"], "tool_use");
    drop(hub);
}

#[tokio::test]
async fn messages_stream_restores_tool_use_name_in_content_block_start() {
    let upstream = MockServer::start().await;
    let (hub, base) = hub_with_echo(&upstream, true).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_with_tool(true))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    let events = parse_sse(&text);
    let start = events
        .iter()
        .find(|(name, _)| name == "content_block_start")
        .expect("a tool block opened");
    assert_eq!(start.1["content_block"]["type"], "tool_use");
    assert_eq!(start.1["content_block"]["name"], LONG_TOOL);
    let alias = sent_alias(&upstream).await;
    assert!(
        !text.contains(&alias),
        "no alias reaches the client: {text}"
    );
    drop(hub);
}

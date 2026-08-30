//! Header-driven request transforms, end to end: what actually reaches the
//! upstream, and what the client gets back.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use wiremock::matchers::{method, path};
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

/// A hub with one profile pointed at `upstream`, plus any extra env.
async fn hub_for(upstream: &MockServer, extra: &[(&str, &str)]) -> (HubProcess, String) {
    let mut vars = HashMap::from([
        ("LLM_HUB_PROFILES".to_string(), "up".to_string()),
        ("LLM_HUB_UP_BASE_URL".to_string(), upstream.uri()),
    ]);
    for (key, value) in extra {
        vars.insert((*key).to_string(), (*value).to_string());
    }
    start_hub(vars).await
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }))
}

/// Two `OpenAI` SSE chunks; the first delta deliberately omits `role`.
fn roleless_stream() -> ResponseTemplate {
    let body = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn mount_ok(upstream: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(template)
        .mount(upstream)
        .await;
}

/// 77 bytes — the MCP double-prefix shape that OpenAI rejects outright.
const LONG_TOOL: &str =
    "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";

/// An upstream that answers with a tool call named exactly what it was asked
/// to call, which is what makes the round trip a real test: nothing here knows
/// how the alias is derived, only that the client must never see it.
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
        if self.stream {
            let sse = format!(
                concat!(
                    "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"tool_calls\":[{{\"index\":0,\"id\":\"call_1\",\"function\":{{\"name\":\"{name}\",\"arguments\":\"\"}}}}]}}}}]}}\n\n",
                    "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":\"tool_calls\"}}]}}\n\n",
                    "data: [DONE]\n\n",
                ),
                name = name
            );
            return ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": serde_json::Value::Null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": name, "arguments": "{}"}}],
            }}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        }))
    }
}

fn chat_with_tool(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "up/gpt-main",
        "stream": stream,
        "messages": [{"role": "user", "content": "list the reviews"}],
        "tools": [{"type": "function", "function": {
            "name": LONG_TOOL, "description": "d", "parameters": {"type": "object"}
        }}],
        "tool_choice": {"type": "function", "function": {"name": LONG_TOOL}},
    })
}

/// The alias the hub sent upstream, asserted to be a legal function name.
fn sent_alias(sent: &Request) -> String {
    let body = upstream_body(sent);
    let alias = body["tools"][0]["function"]["name"]
        .as_str()
        .expect("tool name is a string")
        .to_string();
    assert_eq!(alias.len(), 64, "aliased to the provider's limit: {alias}");
    assert_ne!(alias, LONG_TOOL);
    alias
}

/// Every chat-completions request the upstream saw. The hub also fans out to
/// `/models` on boot, so the raw recording is not just the proxied calls.
async fn completions(upstream: &MockServer) -> Vec<Request> {
    upstream
        .received_requests()
        .await
        .expect("recording enabled")
        .into_iter()
        .filter(|request| request.url.path() == "/chat/completions")
        .collect()
}

async fn only_request(upstream: &MockServer) -> Request {
    let mut requests = completions(upstream).await;
    assert_eq!(requests.len(), 1, "exactly one upstream attempt");
    requests.remove(0)
}

fn upstream_body(request: &Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).expect("upstream body is json")
}

fn chat_with_system(system: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "up/gpt-main",
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": "hi"},
        ]
    })
}

#[tokio::test]
async fn system_prompt_drop_reaches_upstream_without_system_message() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, ok_completion()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-system-prompt-mode", "DROP")
        .json(&chat_with_system("be terse"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-transforms"],
        "system-prompt=drop"
    );

    let sent = only_request(&upstream).await;
    let messages = upstream_body(&sent)["messages"].as_array().unwrap().clone();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert!(
        sent.headers.get("x-llm-hub-system-prompt-mode").is_none(),
        "hub control headers are not forwarded upstream"
    );
    drop(hub);
}

#[tokio::test]
async fn system_prompt_truncate_reaches_upstream_capped() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, ok_completion()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-system-prompt-mode", "truncate")
        .json(&chat_with_system(&"s".repeat(4000)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-transforms"],
        "system-prompt=truncate"
    );

    let sent = only_request(&upstream).await;
    let system = upstream_body(&sent)["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(system.chars().count(), 1000);
    drop(hub);
}

#[tokio::test]
async fn reasoning_strip_header_removes_reasoning_keys_upstream() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, ok_completion()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-reasoning-strip", "true")
        .json(&serde_json::json!({
            "model": "up/gpt-main",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "thinking": {"type": "enabled"},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-transforms"],
        "reasoning-strip"
    );

    let sent = only_request(&upstream).await;
    let body = upstream_body(&sent);
    let obj = body.as_object().unwrap();
    assert!(!obj.contains_key("reasoning_effort"));
    assert!(!obj.contains_key("thinking"));
    assert!(obj.contains_key("messages"));
    drop(hub);
}

#[tokio::test]
async fn unrecognized_and_absent_headers_leave_the_upstream_request_untouched() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, ok_completion()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let sent_body = chat_with_system(&"s".repeat(4000));
    for header in [None, Some("shorten"), Some("")] {
        let mut request = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&sent_body);
        if let Some(value) = header {
            request = request.header("x-llm-hub-system-prompt-mode", value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            response.headers().get("x-llm-hub-transforms").is_none(),
            "no transform fired, so no diagnostic header"
        );
    }

    for received in completions(&upstream).await {
        let body = upstream_body(&received);
        // Only the model is rewritten (profile prefix stripped for the upstream).
        assert_eq!(body["messages"], sent_body["messages"]);
        assert_eq!(body["model"], "gpt-main");
    }
    drop(hub);
}

#[tokio::test]
async fn stream_role_is_injected_into_the_first_sse_chunk() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, roleless_stream()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "up/gpt-main", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    let frames: Vec<&str> = body
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .collect();
    assert_eq!(frames.len(), 3, "frames: {frames:?}");
    assert!(
        frames[0].contains("\"role\":\"assistant\""),
        "first delta repaired: {}",
        frames[0]
    );
    assert!(
        !frames[1].contains("\"role\""),
        "later frames untouched: {}",
        frames[1]
    );
    assert_eq!(frames[2], "data: [DONE]");

    let sent = only_request(&upstream).await;
    assert_eq!(
        sent.headers["accept-encoding"], "identity",
        "a rewritten body must arrive uncompressed"
    );
    drop(hub);
}

#[tokio::test]
async fn stream_role_env_kill_switch_disables_injection() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, roleless_stream()).await;
    let (hub, base) = hub_for(&upstream, &[("LLM_HUB_STREAM_ROLE", "false")]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "up/gpt-main", "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(!body.contains("\"role\""), "untouched stream: {body}");
    drop(hub);
}

#[tokio::test]
async fn non_streaming_requests_keep_the_untransformed_passthrough() {
    let upstream = MockServer::start().await;
    mount_ok(&upstream, ok_completion()).await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("accept-encoding", "gzip")
        .json(&serde_json::json!({
            "model": "up/gpt-main",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");

    let sent = only_request(&upstream).await;
    assert_eq!(
        sent.headers["accept-encoding"], "gzip",
        "no transform, so the caller's encoding preference is forwarded as-is"
    );
    drop(hub);
}

#[tokio::test]
async fn long_tool_name_is_truncated_upstream_and_restored_in_the_response() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(EchoToolName { stream: false })
        .mount(&upstream)
        .await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_with_tool(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-transforms"], "tool-names=1");
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["name"], LONG_TOOL,
        "the client only ever sees its own name"
    );

    let sent = only_request(&upstream).await;
    sent_alias(&sent);
    assert_eq!(
        sent.headers["accept-encoding"], "identity",
        "a rewritten body must arrive uncompressed"
    );
    drop(hub);
}

#[tokio::test]
async fn tool_choice_alias_matches_the_tools_array_alias_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(EchoToolName { stream: false })
        .mount(&upstream)
        .await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_with_tool(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let sent = only_request(&upstream).await;
    let alias = sent_alias(&sent);
    // A forced choice pointing at a name the tools array no longer declares is
    // an upstream 400; determinism is what keeps the two in step.
    assert_eq!(
        upstream_body(&sent)["tool_choice"]["function"]["name"],
        alias
    );
    drop(hub);
}

#[tokio::test]
async fn long_tool_name_is_restored_across_sse_frames_in_a_stream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(EchoToolName { stream: true })
        .mount(&upstream)
        .await;
    let (hub, base) = hub_for(&upstream, &[]).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_with_tool(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();

    let sent = only_request(&upstream).await;
    let alias = sent_alias(&sent);
    assert!(body.contains(LONG_TOOL), "restored in the stream: {body}");
    assert!(
        !body.contains(&alias),
        "no alias reaches the client: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    drop(hub);
}

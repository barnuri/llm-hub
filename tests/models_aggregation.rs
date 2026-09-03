//! Partial aggregation: live upstreams merge, a dead upstream degrades to
//! its static list — never a 502.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn start_hub(vars: HashMap<String, String>) -> (HubProcess, String) {
    let port = free_port();
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
    wait_ready(&base).await;
    (HubProcess(child), base)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_ready(base: &str) {
    let client = reqwest::Client::new();
    for _ in 0..50 {
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("hub did not become ready");
}

fn models_response(ids: &[&str]) -> ResponseTemplate {
    let data: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "upstream"}))
        .collect();
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": data}))
}

#[tokio::test]
async fn aggregates_partially_when_one_upstream_is_down() {
    let alive_a = MockServer::start().await;
    let alive_b = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(models_response(&[
            "gpt-4o",
            "bedrock/anthropic.claude-opus-5",
        ]))
        .mount(&alive_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(models_response(&["meta-llama/Llama-4-Scout"]))
        .mount(&alive_b)
        .await;

    let dead_port = free_port();
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "a,b,dead".into()),
        ("LLM_HUB_A_BASE_URL".into(), alive_a.uri()),
        ("LLM_HUB_B_BASE_URL".into(), alive_b.uri()),
        (
            "LLM_HUB_DEAD_BASE_URL".into(),
            format!("http://127.0.0.1:{dead_port}"),
        ),
        ("LLM_HUB_DEAD_MODELS".into(), "static-x".into()),
    ]))
    .await;

    let response = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(
        response.status(),
        200,
        "dead upstream must never cause a 502"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();

    assert!(ids.contains(&"a/gpt-4o"));
    assert!(
        ids.contains(&"a/bedrock/anthropic.claude-opus-5[1m]"),
        "1M Claude families are advertised with [1m] so Claude Code assumes 1M"
    );
    let opus = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "a/bedrock/anthropic.claude-opus-5[1m]")
        .unwrap();
    assert_eq!(opus["max_input_tokens"], 1_000_000);
    assert!(
        ids.contains(&"b/meta-llama/Llama-4-Scout"),
        "slash-containing upstream id keeps full path"
    );
    assert!(
        ids.contains(&"dead/static-x"),
        "dead upstream degrades to its static list"
    );
    drop(hub);
}

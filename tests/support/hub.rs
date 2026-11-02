//! Starts the llm-hub binary for integration tests on a free local port.
#![allow(dead_code)] // each test file uses only part of this module

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use wiremock::{Match, MockServer, Request, ResponseTemplate};

/// Kills the hub process when the test ends.
pub struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the hub with `vars` set and returns it with its base URL once `/healthz` answers.
pub async fn start_hub(vars: HashMap<String, String>) -> (HubProcess, String) {
    let port = free_port();
    let dir = tempfile::tempdir().unwrap().keep();
    let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
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

/// A local port that nothing listens on yet.
pub fn free_port() -> u16 {
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

/// Matches a request whose body does not contain `needle`, so solver and judge mocks never overlap.
pub struct BodyLacks(pub &'static str);

impl Match for BodyLacks {
    fn matches(&self, request: &Request) -> bool {
        !String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

/// Matches a request whose body contains `needle`, so a judge mock never answers a solver call.
pub struct BodyHas(pub &'static str);

impl Match for BodyHas {
    fn matches(&self, request: &Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(self.0)
    }
}

/// A chat completion whose first choice says `text`.
pub fn completion(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion", "model": "upstream",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
}

/// A hub with profiles `alpha` and `beta`, after its first model refresh, so the Smart Auto pool holds both.
pub async fn start_alpha_beta_hub(alpha: &MockServer, beta: &MockServer) -> (HubProcess, String) {
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "alpha,beta".into()),
        ("LLM_HUB_ALPHA_BASE_URL".into(), alpha.uri()),
        ("LLM_HUB_BETA_BASE_URL".into(), beta.uri()),
    ]))
    .await;
    let models: serde_json::Value = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let profile_models = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|model| model["owned_by"] != "hub")
        .count();
    assert_eq!(profile_models, 2, "{models}");
    (hub, base)
}

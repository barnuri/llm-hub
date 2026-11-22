//! Starts the llm-hub binary for integration tests on a free local port.
#![allow(dead_code, reason = "Each test file uses only part of this module")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Starts the hub with `vars` set and returns it with its base URL once it logs its own bind.
/// A hub that exits before it is ready lost its port to another listener, so the next port is tried.
pub async fn start_hub(vars: HashMap<String, String>) -> (HubProcess, String) {
    for _ in 0..START_ATTEMPTS {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap().keep();
        let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
        command
            .arg("serve")
            .current_dir(&dir)
            .env("LLM_HUB_PORT", port.to_string())
            .env("LLM_HUB_AUTO_UPDATE", "0")
            .env("LLM_HUB_NOTIFY_DESKTOP", "false")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &vars {
            command.env(key, value);
        }
        let mut hub = HubProcess(command.spawn().expect("hub starts"));
        let listening = Arc::new(AtomicBool::new(false));
        let needle = format!("listening on http://127.0.0.1:{port}");
        if let Some(stdout) = hub.0.stdout.take() {
            drain_log(stdout, needle.clone(), Arc::clone(&listening));
        }
        if let Some(stderr) = hub.0.stderr.take() {
            drain_log(stderr, needle, Arc::clone(&listening));
        }
        let base = format!("http://127.0.0.1:{port}");
        if wait_ready(&base, &mut hub.0, &listening).await {
            return (hub, base);
        }
    }
    panic!("hub did not start on {START_ATTEMPTS} ports");
}

/// Reads a hub log stream until it closes and sets `listening` when a line names this hub's bind.
fn drain_log(stream: impl Read + Send + 'static, needle: String, listening: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if line.contains(&needle) {
                listening.store(true, Ordering::SeqCst);
            }
        }
    });
}

const START_ATTEMPTS: usize = 5;

/// A local port that nothing listens on yet.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// True once the hub logs its bind and `/healthz` answers `ok` while `child` still runs. False when `child` exits first.
/// A `/healthz` answer alone can come from another test's hub on the same port, so the log line decides.
async fn wait_ready(base: &str, child: &mut Child, listening: &AtomicBool) -> bool {
    let client = reqwest::Client::new();
    // A loaded machine can take well over 5 s to start a hub; allow 30 s before failing.
    for _ in 0..300 {
        if child.try_wait().unwrap().is_some() {
            return false;
        }
        if listening.load(Ordering::SeqCst)
            && let Ok(response) = client.get(format!("{base}/healthz")).send().await
            && response.status().is_success()
            && response.text().await.is_ok_and(|text| text == "ok")
        {
            return child.try_wait().unwrap().is_none();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("hub did not become ready within 30 s");
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
    start_alpha_beta_hub_with(alpha, beta, HashMap::new()).await
}

/// [`start_alpha_beta_hub`] with extra environment variables.
pub async fn start_alpha_beta_hub_with(
    alpha: &MockServer,
    beta: &MockServer,
    extra: HashMap<String, String>,
) -> (HubProcess, String) {
    let mut vars = HashMap::from([
        ("LLM_HUB_PROFILES".into(), "alpha,beta".into()),
        ("LLM_HUB_ALPHA_BASE_URL".into(), alpha.uri()),
        ("LLM_HUB_BETA_BASE_URL".into(), beta.uri()),
    ]);
    vars.extend(extra);
    let (hub, base) = start_hub(vars).await;
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

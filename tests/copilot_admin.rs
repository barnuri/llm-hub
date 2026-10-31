//! Admin API guard for profiles that authenticate with `LLM_HUB_<NAME>_AUTH`.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start_copilot_hub(dir: &Path) -> (HubProcess, String) {
    let port = free_port();
    let unreachable_api = format!("http://127.0.0.1:{}", free_port());
    let child = Command::new(env!("CARGO_BIN_EXE_llm-hub"))
        .current_dir(dir)
        .env("LLM_HUB_PORT", port.to_string())
        .env("LLM_HUB_AUTO_UPDATE", "0")
        .env("LLM_HUB_PROFILES", "copilot")
        .env("LLM_HUB_COPILOT_BASE_URL", "https://api.githubcopilot.com")
        .env("LLM_HUB_COPILOT_AUTH", "github-copilot")
        .env("LLM_HUB_COPILOT_GITHUB_TOKEN_SOURCE", "gh")
        .env("LLM_HUB_COPILOT_GH_PATH", dir.join("missing-gh"))
        .env("LLM_HUB_COPILOT_GITHUB_API_URL", unreachable_api)
        .env_remove("GH_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .spawn()
        .expect("hub starts");
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

#[tokio::test]
async fn upsert_with_api_key_on_copilot_profile_is_rejected_and_env_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, base) = start_copilot_hub(dir.path()).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/api/profiles"))
        .json(&serde_json::json!({
            "name": "copilot",
            "base_url": "https://api.githubcopilot.com",
            "api_key": "sk-test",
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body = response.text().await.unwrap();
    assert!(body.contains("LLM_HUB_COPILOT_AUTH"), "{body}");
    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap_or_default();
    assert!(!env.contains("API_KEY"), "{env}");
    drop(hub);
}

//! Admin API guard for profiles that authenticate with `LLM_HUB_<NAME>_AUTH`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

use std::collections::HashMap;
use std::path::Path;

use crate::hub::{HubProcess, free_port, start_hub_in};

async fn start_copilot_hub(dir: &Path) -> (HubProcess, String) {
    let unreachable_api = format!("http://127.0.0.1:{}", free_port());
    let vars = HashMap::from([
        ("LLM_HUB_PROFILES".to_owned(), "copilot".to_owned()),
        (
            "LLM_HUB_COPILOT_BASE_URL".to_owned(),
            "https://api.githubcopilot.com".to_owned(),
        ),
        (
            "LLM_HUB_COPILOT_AUTH".to_owned(),
            "github-copilot".to_owned(),
        ),
        (
            "LLM_HUB_COPILOT_GITHUB_TOKEN_SOURCE".to_owned(),
            "gh".to_owned(),
        ),
        (
            "LLM_HUB_COPILOT_GH_PATH".to_owned(),
            dir.join("missing-gh").display().to_string(),
        ),
        ("LLM_HUB_COPILOT_GITHUB_API_URL".to_owned(), unreachable_api),
    ]);
    start_hub_in(dir, vars, &["GH_TOKEN", "GH_ENTERPRISE_TOKEN"]).await
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

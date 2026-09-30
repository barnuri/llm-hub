//! `/api/insights` and `/api/insights/pick` over a real hub process with a
//! persistent store and a mocked upstream.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GOOD_CALLS: usize = 5;
const BAD_CALLS: usize = 3;

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
        .env("LLM_HUB_AUTO_UPDATE", "0")
        .env("LLM_HUB_PERSISTENT", "true")
        .env("LLM_HUB_STORE", "sqlite")
        .env("LLM_HUB_STORE_PATH", dir.join("hub.db"));
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

async fn mock_upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "good"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({"model": "bad"})))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "upstream exploded"}})),
        )
        .mount(&upstream)
        .await;
    upstream
}

async fn call_model(client: &reqwest::Client, base: &str, model: &str) {
    client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
}

/// Stats are written by a background task, so poll until every call landed.
async fn insights_when_recorded(client: &reqwest::Client, url: &str, calls: u64) -> Value {
    for _ in 0..50 {
        let body: Value = client.get(url).send().await.unwrap().json().await.unwrap();
        let recorded: u64 = body["models"]
            .as_array()
            .map(|models| models.iter().filter_map(|m| m["requests"].as_u64()).sum())
            .unwrap_or(0);
        if recorded >= calls {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("insights never showed {calls} calls");
}

fn model<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == id)
        .unwrap()
}

#[tokio::test]
async fn insights__rank_the_stable_model_and_flag_the_failing_one() {
    let upstream = mock_upstream().await;
    let (_hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "local".into()),
        ("LLM_HUB_LOCAL_BASE_URL".into(), upstream.uri()),
    ]))
    .await;
    let client = reqwest::Client::new();
    for _ in 0..GOOD_CALLS {
        call_model(&client, &base, "local/good").await;
    }
    for _ in 0..BAD_CALLS {
        call_model(&client, &base, "local/bad").await;
    }

    let report = insights_when_recorded(
        &client,
        &format!("{base}/api/insights?range=1d&min_requests=2"),
        (GOOD_CALLS + BAD_CALLS) as u64,
    )
    .await;

    let good = model(&report, "local/good");
    assert_eq!(good["health"], "healthy");
    assert_eq!(good["ranked"], true);
    assert_eq!(good["success_rate_pct"], 100.0);
    let bad = model(&report, "local/bad");
    assert_eq!(bad["health"], "failing");
    assert_eq!(bad["ranked"], false);
    assert_eq!(bad["top_errors"][0]["reason"], "upstream exploded");
    assert_eq!(report["leaders"]["best_overall"], "local/good");
    assert_eq!(report["models"][0]["model"], "local/good");

    let picked: Value = client
        .get(format!(
            "{base}/api/insights/pick?by=stability&range=1d&min_requests=2"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(picked["model"], "local/good");
    assert_eq!(picked["by"], "stability");

    let agent: Value = client
        .get(format!(
            "{base}/api/insights/report?range=1d&min_requests=2"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(agent["generated_at"].as_str().unwrap().ends_with('Z'));
    assert!(
        agent["summary"][0]
            .as_str()
            .unwrap()
            .starts_with("Best overall: local/good")
    );
    assert_eq!(agent["failure_reasons"][0]["reason"], "upstream exploded");
    assert_eq!(agent["failure_reasons"][0]["count"], BAD_CALLS);
    // A sub-millisecond mock has no measurable throughput, so only the
    // criteria that do not need timing are guaranteed a pick.
    let picked_by: Vec<&str> = agent["picks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|picked| picked["by"].as_str().unwrap())
        .collect();
    assert!(picked_by.contains(&"overall") && picked_by.contains(&"stability"));

    let none = client
        .get(format!("{base}/api/insights/pick?profile=other&range=1d"))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), 404);

    let bad_range = client
        .get(format!("{base}/api/insights?range=2y"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_range.status(), 400);
}

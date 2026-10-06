//! Fallback loop: 429 fails over, 400 never retries, attempts trail reported.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::time::Duration;

use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

fn chat_body(model: &str) -> serde_json::Value {
    serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

fn ok_completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    }))
}

#[tokio::test]
async fn rate_limited_primary_fails_over() {
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
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-fallbacks", "healthy/gpt-fallback")
        .json(&chat_body("limited/gpt-main"))
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
    assert!(
        attempts.contains("healthy/gpt-fallback=200"),
        "attempts trail: {attempts}"
    );
    drop(hub);
}

#[tokio::test]
async fn bad_request_never_retries() {
    let rejecting = MockServer::start().await;
    let never_called = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(serde_json::json!({"error": {"message": "bad"}})),
        )
        .expect(1)
        .mount(&rejecting)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ok_completion())
        .expect(0)
        .mount(&never_called)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "rejecting,fallback".into()),
        ("LLM_HUB_REJECTING_BASE_URL".into(), rejecting.uri()),
        ("LLM_HUB_FALLBACK_BASE_URL".into(), never_called.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-llm-hub-fallbacks", "fallback/gpt-x")
        .json(&chat_body("rejecting/gpt-main"))
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        400,
        "request-shaped errors pass through untouched"
    );
    assert_eq!(response.headers()["x-llm-hub-model"], "rejecting/gpt-main");
    drop(hub);
}

#[tokio::test]
async fn unprefixed_model_is_rejected_with_profile_list() {
    let upstream = MockServer::start().await;
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "solo".into()),
        ("LLM_HUB_SOLO_BASE_URL".into(), upstream.uri()),
    ]))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body("gpt-4o"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("solo"));
    drop(hub);
}

const AUDIO_BOUNDARY: &str = "VoiceNotesBoundary7";

fn transcription_form(model: &str) -> Vec<u8> {
    format!(
        "--{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n\
         --{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.m4a\"\r\n\
         Content-Type: audio/mp4\r\n\r\nAUDIO-BYTES\r\n--{AUDIO_BOUNDARY}--\r\n"
    )
    .into_bytes()
}

async fn post_transcription(base: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={AUDIO_BOUNDARY}"),
        )
        .header("x-llm-hub-fallbacks", "healthy/whisper-fallback")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn multipart_transcription_routes_by_model_field_and_fails_over() {
    let limited = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(body_string_contains("\r\n\r\nwhisper-main\r\n"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&limited)
        .await;
    Mock::given(method("POST"))
        .and(path("/audio/transcriptions"))
        .and(body_string_contains("\r\n\r\nwhisper-fallback\r\n"))
        .and(body_string_contains("AUDIO-BYTES"))
        .and(header(
            "content-type",
            format!("multipart/form-data; boundary={AUDIO_BOUNDARY}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "hi"})))
        .expect(1)
        .mount(&healthy)
        .await;

    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "limited,healthy".into()),
        ("LLM_HUB_LIMITED_BASE_URL".into(), limited.uri()),
        ("LLM_HUB_HEALTHY_BASE_URL".into(), healthy.uri()),
    ]))
    .await;

    let response = post_transcription(&base, transcription_form("limited/whisper-main")).await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-llm-hub-model"],
        "healthy/whisper-fallback"
    );
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["text"],
        "hi"
    );
    drop(hub);
}

#[tokio::test]
async fn multipart_without_model_field_is_rejected() {
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "healthy".into()),
        (
            "LLM_HUB_HEALTHY_BASE_URL".into(),
            "http://127.0.0.1:9".into(),
        ),
    ]))
    .await;

    let without_model = String::from_utf8(transcription_form("x"))
        .unwrap()
        .replace("name=\"model\"", "name=\"prompt\"");
    let response = post_transcription(&base, without_model.into_bytes()).await;

    assert_eq!(response.status(), 400);
    drop(hub);
}

#[tokio::test]
async fn multipart_over_upload_limit_is_rejected() {
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "healthy".into()),
        (
            "LLM_HUB_HEALTHY_BASE_URL".into(),
            "http://127.0.0.1:9".into(),
        ),
    ]))
    .await;

    let oversized_audio = "A".repeat(26 * 1024 * 1024);
    let body = String::from_utf8(transcription_form("healthy/whisper"))
        .unwrap()
        .replace("AUDIO-BYTES", &oversized_audio);
    let response = post_transcription(&base, body.into_bytes()).await;

    assert_eq!(response.status(), 400);
    drop(hub);
}

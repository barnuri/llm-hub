//! Managed local servers: started on first use, shared across concurrent
//! requests, stopped when idle or on hub shutdown, and never settable via the API.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const AUDIO_BOUNDARY: &str = "ManagedBoundary1";

#[cfg(windows)]
const PYTHON: &str = "python";
#[cfg(not(windows))]
const PYTHON: &str = "python3";

const TEST_SERVER: &str = r#"
import http.server, os, sys, threading, time
port, spawn_log = int(sys.argv[1]), sys.argv[2]
with open(spawn_log, "a") as f:
    f.write("spawn\n")
threading.Timer(60, lambda: os._exit(0)).start()
time.sleep(0.5)
class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        body = b'{"text":"hello"}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

/// Stops the hub with SIGTERM so its graceful shutdown path runs. Windows has
/// no SIGTERM, so there the hub is killed outright, which its job object must survive.
struct HubProcess(Child);

impl Drop for HubProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-TERM", &self.0.id().to_string()])
                .status();
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.0.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn start_hub(dir: &Path, vars: HashMap<String, String>) -> (HubProcess, String) {
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
    command
        .current_dir(dir)
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

fn managed_profile(
    server_port: u16,
    command: &str,
    extra: &[(&str, &str)],
) -> HashMap<String, String> {
    let mut vars = HashMap::from([
        ("LLM_HUB_PROFILES".to_string(), "whisper".to_string()),
        (
            "LLM_HUB_WHISPER_BASE_URL".to_string(),
            format!("http://127.0.0.1:{server_port}/v1"),
        ),
        ("LLM_HUB_WHISPER_COMMAND".to_string(), command.to_string()),
        (
            "LLM_HUB_WHISPER_MODELS".to_string(),
            "whisper-large-v3".to_string(),
        ),
    ]);
    for (key, value) in extra {
        vars.insert(format!("LLM_HUB_WHISPER_{key}"), (*value).to_string());
    }
    vars
}

fn test_server_command(dir: &Path, server_port: u16) -> (String, PathBuf) {
    let script = dir.join("server.py");
    std::fs::write(&script, TEST_SERVER).unwrap();
    let spawn_log = dir.join("spawns.txt");
    let command = format!(
        r#"{PYTHON} "{}" {server_port} "{}""#,
        script.display(),
        spawn_log.display()
    );
    (command, spawn_log)
}

fn spawn_count(spawn_log: &Path) -> usize {
    std::fs::read_to_string(spawn_log)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn is_listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

async fn wait_until_not_listening(port: u16, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !is_listening(port) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn transcribe(base: &str) -> reqwest::Response {
    let body = format!(
        "--{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper/whisper-large-v3\r\n\
         --{AUDIO_BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\nAUDIO-BYTES\r\n--{AUDIO_BOUNDARY}--\r\n"
    );
    reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={AUDIO_BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn profile_status(base: &str) -> serde_json::Value {
    let listing: serde_json::Value = reqwest::get(format!("{base}/api/profiles"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    listing["profiles"][0].clone()
}

#[tokio::test]
async fn concurrent_first_requests_share_one_start_and_shutdown_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let server_port = free_port();
    let (command, spawn_log) = test_server_command(dir.path(), server_port);
    let (hub, base) = start_hub(dir.path(), managed_profile(server_port, &command, &[])).await;

    let models: serde_json::Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "whisper/whisper-large-v3");
    assert_eq!(
        spawn_count(&spawn_log),
        0,
        "/v1/models must not start the process"
    );

    let (first, second) = tokio::join!(transcribe(&base), transcribe(&base));

    assert_eq!(first.status(), 200);
    assert_eq!(second.status(), 200);
    assert_eq!(first.text().await.unwrap(), r#"{"text":"hello"}"#);
    assert_eq!(spawn_count(&spawn_log), 1);
    let status = profile_status(&base).await;
    assert_eq!(status["managed"], true);
    assert_eq!(status["running"], true);
    assert!(status.get("command").is_none());
    assert!(dir.path().join("logs/whisper.log").exists());

    drop(hub);
    assert!(
        wait_until_not_listening(server_port, Duration::from_secs(5)).await,
        "managed process outlived the hub"
    );
}

#[tokio::test]
async fn idle_process_is_stopped_and_restarted_on_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let server_port = free_port();
    let (command, spawn_log) = test_server_command(dir.path(), server_port);
    let (hub, base) = start_hub(
        dir.path(),
        managed_profile(server_port, &command, &[("IDLE_SECS", "1")]),
    )
    .await;

    assert_eq!(transcribe(&base).await.status(), 200);
    assert!(
        wait_until_not_listening(server_port, Duration::from_secs(10)).await,
        "idle process was not stopped"
    );
    assert_eq!(profile_status(&base).await["running"], false);

    assert_eq!(transcribe(&base).await.status(), 200);
    assert_eq!(spawn_count(&spawn_log), 2);
    drop(hub);
}

#[tokio::test]
async fn start_timeout_fails_the_attempt_and_names_profile_and_log() {
    let dir = tempfile::tempdir().unwrap();
    let server_port = free_port();
    let (hub, base) = start_hub(
        dir.path(),
        managed_profile(
            server_port,
            &format!(r#"{PYTHON} -c "import time; time.sleep(30)""#),
            &[("START_TIMEOUT_SECS", "1")],
        ),
    )
    .await;

    let response = transcribe(&base).await;

    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("profile whisper"), "{message}");
    assert!(message.contains("whisper.log"), "{message}");
    assert!(message.contains("within 1s"), "{message}");
    drop(hub);
}

#[tokio::test]
async fn port_already_served_is_used_without_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let already_running = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("external"))
        .expect(1)
        .mount(&already_running)
        .await;
    let spawn_marker = dir.path().join("spawned");
    let command = format!(
        r#"{PYTHON} -c "import sys; open(sys.argv[1], 'a')" "{}""#,
        spawn_marker.display()
    );
    let (hub, base) = start_hub(
        dir.path(),
        managed_profile(already_running.address().port(), &command, &[]),
    )
    .await;

    let response = transcribe(&base).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "external");
    assert!(
        !spawn_marker.exists(),
        "the command ran despite a live port"
    );
    drop(hub);
}

#[tokio::test]
async fn admin_api_cannot_set_a_command() {
    let dir = tempfile::tempdir().unwrap();
    let spawn_marker = dir.path().join("pwned");
    let (hub, base) = start_hub(dir.path(), HashMap::new()).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/api/profiles"))
        .json(&serde_json::json!({
            "name": "evil",
            "base_url": format!("http://127.0.0.1:{}/v1", free_port()),
            "command": format!("touch {}", spawn_marker.display()),
            "COMMAND": format!("touch {}", spawn_marker.display()),
            "idle_secs": 0,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
    assert!(!env.contains("COMMAND"), "{env}");
    assert!(!env.contains("IDLE_SECS"), "{env}");
    assert_eq!(profile_status(&base).await["managed"], false);

    let chat = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "evil/x", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 502);
    assert!(!spawn_marker.exists());
    drop(hub);
}

//! Integration coverage for the native tuner: a seeded `stream_stall` proposal is approved over HTTP,
//! the registry row gains `--reasoning off`, a backup is written, only that model restarts, and the
//! other running model keeps its process.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const SERVER: &str = r#"#!/usr/bin/env python3
import http.server, os, sys
args=sys.argv[1:]
port=int(args[args.index('--port')+1])
with open(os.environ['TUNER_SPAWNS'],'a') as f:
    f.write(str(os.getpid())+' '+' '.join(args)+'\n')
class H(http.server.BaseHTTPRequestHandler):
  def do_GET(self):
    body=b'ok'; self.send_response(200); self.send_header('content-length',str(len(body))); self.end_headers(); self.wfile.write(body)
  def do_POST(self):
    self.rfile.read(int(self.headers.get('content-length','0')))
    body=b'{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}'
    self.send_response(200); self.send_header('content-type','application/json'); self.send_header('content-length',str(len(body))); self.end_headers(); self.wfile.write(body)
  def log_message(self,*args): pass
http.server.ThreadingHTTPServer(('127.0.0.1',port),H).serve_forever()
"#;

const REGISTRY: &str = "# id|repo|file|size_gb|ctx|quant|extra_flags\n\
a|org/a|a.gguf|1|4096|Q4|--jinja --reasoning on\n\
b|org/b|b.gguf|1|4096|Q4|--jinja\n";

const PROPOSAL_ID: &str = "tuner-stream-stall-a";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts the hub binary with `vars` and waits until `/healthz` answers.
async fn start_hub(dir: &Path, vars: &[(String, String)], base: &str) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in vars {
        command.env(key, value);
    }
    let mut hub = command.spawn().unwrap();
    let client = reqwest::Client::new();
    for _ in 0..200 {
        assert!(hub.try_wait().unwrap().is_none(), "hub exited during start");
        if client.get(format!("{base}/healthz")).send().await.is_ok() {
            return hub;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("hub did not answer /healthz");
}

fn stop_hub(mut hub: Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &hub.id().to_string()])
        .status();
    let _ = hub.wait();
}

/// Lines of the spawn log that mention `file`, as `(pid, argv)`.
fn spawns_of(log: &Path, file: &str) -> Vec<(String, String)> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(file))
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(pid, argv)| (pid.to_owned(), argv.to_owned()))
        })
        .collect()
}

#[tokio::test]
#[allow(clippy::too_many_lines, reason = "one scenario reads top to bottom")]
async fn approved_stream_stall_restarts_only_its_model() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("fake-llama-server");
    std::fs::write(&bin, SERVER).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let registry = dir.path().join("registry.txt");
    std::fs::write(&registry, REGISTRY).unwrap();
    std::fs::write(dir.path().join("a.gguf"), "x").unwrap();
    std::fs::write(dir.path().join("b.gguf"), "x").unwrap();
    let spawns = dir.path().join("spawns.txt");
    let store = dir.path().join("hub.db");
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let vars: Vec<(String, String)> = [
        ("LLM_HUB_PROFILES", "local".to_owned()),
        ("LLM_HUB_LOCAL_KIND", "local".to_owned()),
        (
            "LLM_HUB_LOCAL_TUNER_REGISTRY",
            registry.display().to_string(),
        ),
        ("LLM_HUB_LOCAL_REGISTRY", registry.display().to_string()),
        ("LLM_HUB_LOCAL_MODELS_DIR", dir.path().display().to_string()),
        ("LLM_HUB_LOCAL_LLAMA_BIN", bin.display().to_string()),
        ("LLM_HUB_LOCAL_START_TIMEOUT_SECS", "5".to_owned()),
        ("LLM_HUB_PERSISTENT", "true".to_owned()),
        ("LLM_HUB_STORE_PATH", store.display().to_string()),
        ("LLM_HUB_PORT", port.to_string()),
        ("LLM_HUB_AUTO_UPDATE", "0".to_owned()),
        ("LLM_HUB_NOTIFY_DESKTOP", "false".to_owned()),
        ("LLM_HUB_NOTIFICATIONS", "true".to_owned()),
        ("TUNER_SPAWNS", spawns.display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();

    // The first start creates hub.db with the real schema. The proposal is seeded while the hub is
    // stopped, because the notifications load once at boot.
    stop_hub(start_hub(dir.path(), &vars, &base).await);
    {
        let conn = rusqlite::Connection::open(&store).unwrap();
        conn.execute(
            "INSERT INTO notifications (id, kind, source, severity, title, body, profile, model, rule,
                diff, actions, status, created_ms, updated_ms, result, muted)
             VALUES (?1, 'proposal', 'tuner', 'warning', 'Disable reasoning after stream stalls',
                'a had stream stalls while --reasoning was present.', 'local', 'a', 'stream_stall',
                ?2, '[\"approve\",\"dismiss\"]', 'pending', 1, 1, NULL, 0)",
            rusqlite::params![
                PROPOSAL_ID,
                r#"{"add":["--reasoning","off"],"remove":["--reasoning"]}"#
            ],
        )
        .unwrap();
    }

    let hub = start_hub(dir.path(), &vars, &base).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_mins(1))
        .build()
        .unwrap();
    // Start both models, so each has a running process before the approval.
    for model in ["local/b", "local/a"] {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": model, "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{model} starts");
    }
    let b_before = spawns_of(&spawns, "b.gguf");
    let a_before = spawns_of(&spawns, "a.gguf");
    assert_eq!(b_before.len(), 1, "b started once");
    assert_eq!(a_before.len(), 1, "a started once");

    let response = client
        .post(format!("{base}/api/notifications/{PROPOSAL_ID}/approve"))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(status.is_success(), "approve answers 200: {status} {body}");
    let approved: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(approved["status"], "applied", "approve applied: {body}");

    let row = std::fs::read_to_string(&registry).unwrap();
    assert!(
        row.contains("a|org/a|a.gguf|1|4096|Q4|--jinja --reasoning off"),
        "registry row gained --reasoning off: {row}"
    );
    assert!(
        row.contains("b|org/b|b.gguf|1|4096|Q4|--jinja"),
        "other row untouched: {row}"
    );
    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("registry.txt.bak-"))
        .collect();
    assert_eq!(backups.len(), 1, "one registry backup: {backups:?}");

    // The restart stops the old process; the next request starts the new one.
    let restarted = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({
            "model": "local/a", "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        restarted.status().is_success(),
        "local/a restarts on demand"
    );
    let a_after = spawns_of(&spawns, "a.gguf");
    assert_eq!(a_after.len(), 2, "a restarted once: {a_after:?}");
    assert!(
        a_after[1].1.contains("--reasoning off"),
        "the new a process gets the new flags: {}",
        a_after[1].1
    );
    assert_ne!(a_after[0].0, a_after[1].0, "a has a new pid");
    let b_after = spawns_of(&spawns, "b.gguf");
    assert_eq!(b_after, b_before, "b keeps its pid and is not restarted");

    stop_hub(hub);
}

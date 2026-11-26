//! Starts a hub for integration tests on a free local port. [`start_hub`] runs it inside the test
//! process, on its own runtime and thread, so a test pays no process start. [`start_hub_in`] runs the
//! real binary, for the tests of its working directory, signals, command line and child processes.
#![allow(dead_code, reason = "Each test file uses only part of this module")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use wiremock::{Match, MockServer, Request, ResponseTemplate};

/// Kills the hub process when the test ends. The hub also exits by itself when the test process
/// dies first: [`hub_command`] gives it a stdin pipe that closes with this process.
pub struct HubProcess(pub Child);

impl HubProcess {
    /// The hub's process id.
    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// Closes the hub's stdin, so it shuts down gracefully and stops its children, and waits up to
    /// `within` for it to exit. Works the same on every platform. Returns whether it exited in time.
    pub fn stop_gracefully(&mut self, within: Duration) -> bool {
        drop(self.0.stdin.take());
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.0.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

impl Drop for HubProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `llm-hub serve` on `port` in `dir`, with auto-update and desktop popups off and the
/// parent-death guard on: the hub stops when its stdin pipe closes, which happens when the test
/// process exits for any reason. Keep the `Child` (it owns the pipe) for as long as the hub must run.
pub fn hub_command(dir: &std::path::Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
    command
        .arg("serve")
        .current_dir(dir)
        .env("LLM_HUB_PORT", port.to_string())
        .env("LLM_HUB_AUTO_UPDATE", "0")
        .env("LLM_HUB_NOTIFY_DESKTOP", "false")
        .env("LLM_HUB_EXIT_ON_STDIN_EOF", "1")
        .env("LLM_HUB_MANAGED_IDLE_CHECK_INTERVAL_MS", "50")
        .env("LLM_HUB_ENV_WATCH_INTERVAL_MS", "100")
        .env("LLM_HUB_REGISTRY_POLL_INTERVAL_MS", "50")
        .env("LLM_HUB_MANAGED_STOP_GRACE_MS", "300")
        .env("LLM_HUB_COPILOT_PRICES_REFRESH_SECS", "0")
        .stdin(Stdio::piped());
    // A developer's own hub settings must not leak into a test hub.
    for (key, _) in std::env::vars() {
        if key.starts_with("LLM_HUB_") {
            command.env_remove(key);
        }
    }
    command
}

/// A hub that runs inside the test process, on its own thread and runtime, so it outlives the
/// runtime of the test that started it. It sees only its variables, never the process environment.
/// Dropping it asks it to stop. The thread ends with the process when a connection keeps it open.
pub struct InProcessHub {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for InProcessHub {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Starts an in-process hub in a new temporary folder with `vars` set. Returns it with its base URL.
pub async fn start_hub(vars: HashMap<String, String>) -> (InProcessHub, String) {
    let dir = tempfile::tempdir().unwrap().keep();
    start_hub_with_dir(&dir, vars).await
}

/// [`start_hub`] with `dir` as the hub's `.env`, logs and rules folder.
pub async fn start_hub_with_dir(
    dir: &std::path::Path,
    mut vars: HashMap<String, String>,
) -> (InProcessHub, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for (key, value) in [
        ("LLM_HUB_AUTO_UPDATE", "0"),
        ("LLM_HUB_NOTIFY_DESKTOP", "false"),
        ("LLM_HUB_MANAGED_IDLE_CHECK_INTERVAL_MS", "50"),
        ("LLM_HUB_ENV_WATCH_INTERVAL_MS", "100"),
        ("LLM_HUB_REGISTRY_POLL_INTERVAL_MS", "50"),
        ("LLM_HUB_MANAGED_STOP_GRACE_MS", "300"),
        ("LLM_HUB_COPILOT_PRICES_REFRESH_SECS", "0"),
    ] {
        vars.entry(key.to_owned())
            .or_insert_with(|| value.to_owned());
    }
    vars.insert("LLM_HUB_PORT".to_owned(), port.to_string());
    let runtime = llm_hub::HubRuntime::in_dir(dir, vars);
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("test-hub".to_owned())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let served = rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                llm_hub::Hub::new()
                    .serve_on(listener, runtime, async {
                        let _ = stopped.await;
                    })
                    .await
            });
            rt.shutdown_background();
            if let Err(error) = served {
                eprintln!("test hub failed: {error}");
            }
        })
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    wait_healthy(&base).await;
    (
        InProcessHub {
            shutdown: Some(shutdown),
        },
        base,
    )
}

/// Waits until `/healthz` answers `ok`. The listener is bound before the hub starts, so an
/// answer can only come from this hub.
async fn wait_healthy(base: &str) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(response) = client.get(format!("{base}/healthz")).send().await
            && response.status().is_success()
            && response.text().await.is_ok_and(|text| text == "ok")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("hub did not become ready within {READY_TIMEOUT:?}");
}

/// Starts the real binary in a new temporary folder with `vars` set, for a test that needs a real
/// process: the working directory, signals, the command line or child processes.
pub async fn start_process_hub(vars: HashMap<String, String>) -> (HubProcess, String) {
    let dir = tempfile::tempdir().unwrap().keep();
    start_hub_in(&dir, vars, &[]).await
}

/// [`start_process_hub`] with `dir` as the working directory and the `removed` variables taken out of the
/// inherited environment.
pub async fn start_hub_in(
    dir: &std::path::Path,
    vars: HashMap<String, String>,
    removed: &[&str],
) -> (HubProcess, String) {
    for _ in 0..START_ATTEMPTS {
        let port = free_port();
        let mut command = hub_command(dir, port);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        for key in removed {
            command.env_remove(key);
        }
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
const READY_TIMEOUT: Duration = Duration::from_secs(30);

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
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    // A loaded machine can take well over 5 s to start a hub, so the limit is wall-clock time.
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
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
    panic!("hub did not become ready within {READY_TIMEOUT:?}");
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
pub async fn start_alpha_beta_hub(alpha: &MockServer, beta: &MockServer) -> (InProcessHub, String) {
    start_alpha_beta_hub_with(alpha, beta, HashMap::new()).await
}

/// [`start_alpha_beta_hub`] with extra environment variables.
pub async fn start_alpha_beta_hub_with(
    alpha: &MockServer,
    beta: &MockServer,
    extra: HashMap<String, String>,
) -> (InProcessHub, String) {
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

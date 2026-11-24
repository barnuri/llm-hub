//! Local model serving: the hub starts one fake llama-server per model on demand.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Owns one test's hub, temp dir, fake server and spawn log. Dropping it stops every child.
struct Fixture {
    dir: tempfile::TempDir,
    hub: Option<Child>,
    base: String,
    client: reqwest::Client,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut hub) = self.hub.take() {
            terminate_hub(&mut hub);
        }
        for pid in self.spawned_pids() {
            kill_if_alive(pid);
        }
    }
}

impl Fixture {
    /// Starts a hub with `local` as a local profile over `rows` (id, size GB). Extra vars override defaults.
    async fn start(rows: &[(&str, f64)], extra: &[(&str, &str)]) -> Fixture {
        let (mut fixture, mut vars) = Fixture::prepare(rows);
        for (key, value) in extra {
            vars.insert((*key).to_owned(), (*value).to_owned());
        }
        fixture.launch(&vars).await;
        fixture
    }

    /// Starts a hub with no local profile.
    async fn start_default() -> Fixture {
        let (mut fixture, _) = Fixture::prepare(&[]);
        fixture.launch(&HashMap::new()).await;
        fixture
    }

    /// Writes the registry, the model files and the fake server, and returns the hub vars.
    fn prepare(rows: &[(&str, f64)]) -> (Fixture, HashMap<String, String>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("models")).unwrap();
        let mut registry = String::from("# id|repo|file|size_gb|ctx\n");
        for (id, size_gb) in rows {
            writeln!(registry, "{id}|org/{id}|{id}.gguf|{size_gb}|4096").unwrap();
            std::fs::write(root.join(format!("models/{id}.gguf")), "x").unwrap();
        }
        std::fs::write(root.join("registry.txt"), registry).unwrap();
        let bin = std::path::Path::new(env!("CARGO_BIN_EXE_test-local-server"));

        let vars = HashMap::from([
            ("LLM_HUB_PROFILES".to_owned(), "local".to_owned()),
            ("LLM_HUB_LOCAL_KIND".to_owned(), "local".to_owned()),
            (
                "LLM_HUB_LOCAL_REGISTRY".to_owned(),
                path_str(&root.join("registry.txt")),
            ),
            (
                "LLM_HUB_LOCAL_MODELS_DIR".to_owned(),
                path_str(&root.join("models")),
            ),
            (
                "LLM_HUB_LOCAL_HF_CACHE".to_owned(),
                path_str(&root.join("hf")),
            ),
            ("LLM_HUB_LOCAL_LLAMA_BIN".to_owned(), path_str(bin)),
            (
                "LLM_HUB_LOCAL_START_TIMEOUT_SECS".to_owned(),
                "20".to_owned(),
            ),
            ("SPAWN_LOG".to_owned(), path_str(&root.join("spawns.txt"))),
        ]);
        let fixture = Fixture {
            dir,
            hub: None,
            base: String::new(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_mins(1))
                .build()
                .unwrap(),
        };
        (fixture, vars)
    }

    async fn launch(&mut self, vars: &HashMap<String, String>) {
        for _ in 0..5 {
            let port = free_port();
            let mut command = Command::new(env!("CARGO_BIN_EXE_llm-hub"));
            command
                .arg("serve")
                .current_dir(self.dir.path())
                .env("LLM_HUB_PORT", port.to_string())
                .env("LLM_HUB_AUTO_UPDATE", "0")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (key, value) in vars {
                command.env(key, value);
            }
            let mut hub = command.spawn().expect("hub starts");
            let base = format!("http://127.0.0.1:{port}");
            if wait_ready(&self.client, &base, &mut hub).await {
                self.hub = Some(hub);
                self.base = base;
                return;
            }
        }
        panic!("hub did not start on 5 ports");
    }

    /// Stops the hub with SIGTERM, so its graceful shutdown stops the children.
    fn stop_hub(&mut self) {
        if let Some(mut hub) = self.hub.take() {
            terminate_hub(&mut hub);
        }
    }

    fn url(&self, route: &str) -> String {
        format!("{}{route}", self.base)
    }

    async fn models(&self) -> Vec<String> {
        let body: serde_json::Value = self
            .client
            .get(self.url("/v1/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|model| model["id"].as_str().map(str::to_owned))
            .collect()
    }

    /// Sends one chat request for `local/<id>` and returns its status.
    async fn chat(&self, id: &str) -> reqwest::StatusCode {
        let body = serde_json::json!({
            "model": format!("local/{id}"),
            "messages": [{"role": "user", "content": "hi"}],
        });
        self.client
            .post(self.url("/v1/chat/completions"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    /// `(pid, model file name, port)` of every process the fake server recorded.
    fn spawns(&self) -> Vec<(u32, String, u16)> {
        let text = std::fs::read_to_string(self.dir.path().join("spawns.txt")).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                Some((
                    parts.next()?.parse().ok()?,
                    parts.next()?.to_owned(),
                    parts.next()?.parse().ok()?,
                ))
            })
            .collect()
    }

    fn spawned_pids(&self) -> Vec<u32> {
        self.spawns().into_iter().map(|(pid, _, _)| pid).collect()
    }

    fn spawn_count(&self, id: &str) -> usize {
        self.spawns()
            .iter()
            .filter(|(_, model, _)| *model == format!("{id}.gguf"))
            .count()
    }

    fn pid_of(&self, id: &str) -> u32 {
        self.spawns()
            .iter()
            .rev()
            .find_map(|(pid, model, _)| (*model == format!("{id}.gguf")).then_some(*pid))
            .unwrap_or_else(|| panic!("no spawn of {id}"))
    }

    fn log(&self, id: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(format!("logs/local/{id}.log")))
            .unwrap_or_default()
    }

    fn write_registry(&self, rows: &[(&str, f64)]) {
        let mut registry = String::from("# id|repo|file|size_gb|ctx\n");
        for (id, size_gb) in rows {
            writeln!(registry, "{id}|org/{id}|{id}.gguf|{size_gb}|4096").unwrap();
            std::fs::write(self.dir.path().join(format!("models/{id}.gguf")), "x").unwrap();
        }
        std::fs::write(self.dir.path().join("registry.txt"), registry).unwrap();
    }
}

fn path_str(path: &Path) -> String {
    path.display().to_string()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// True once `/healthz` answers `ok` while `hub` still runs.
async fn wait_ready(client: &reqwest::Client, base: &str, hub: &mut Child) -> bool {
    for _ in 0..3000 {
        if hub.try_wait().unwrap().is_some() {
            return false;
        }
        if let Ok(response) = client.get(format!("{base}/healthz")).send().await
            && response.status().is_success()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("hub did not become ready within 30 s");
}

/// Sends SIGTERM to the hub and waits up to 10 s for its graceful shutdown, then kills it.
fn terminate_hub(hub: &mut Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &hub.id().to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = hub.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = hub.kill();
    let _ = hub.wait();
}

fn is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only checks that the process exists and touches no memory.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn kill_if_alive(pid: u32) {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return;
    };
    if is_alive(pid) {
        // SAFETY: SIGKILL to a fake llama-server pid this test spawned; it touches no memory.
        unsafe {
            libc::kill(raw, libc::SIGKILL);
        }
    }
}

/// Polls `check` until it returns true or `timeout` passes.
async fn eventually(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    check()
}

#[tokio::test]
async fn model_list_shows_local_models_without_starting_a_process() {
    let fx = Fixture::start(&[("a", 1.0), ("b", 1.0)], &[]).await;

    let models = fx.models().await;

    assert!(models.contains(&"local/a".to_owned()), "{models:?}");
    assert!(models.contains(&"local/b".to_owned()), "{models:?}");
    assert!(fx.spawns().is_empty(), "listing started a process");
}

#[tokio::test]
async fn concurrent_requests_share_one_start() {
    let fx = Fixture::start(&[("a", 1.0)], &[]).await;

    let statuses = futures_util::future::join_all((0..5).map(|_| fx.chat("a"))).await;

    assert!(
        statuses.iter().all(reqwest::StatusCode::is_success),
        "{statuses:?}"
    );
    assert_eq!(fx.spawn_count("a"), 1, "{:?}", fx.spawns());
}

#[tokio::test]
async fn stderr_lands_in_the_model_log_file() {
    let fx = Fixture::start(&[("a", 1.0)], &[]).await;
    assert!(fx.chat("a").await.is_success());

    let logged = eventually(Duration::from_secs(5), || {
        fx.log("a").contains("fake llama stderr")
    })
    .await;

    assert!(logged, "log: {}", fx.log("a"));
}

#[tokio::test]
async fn a_registry_edit_adds_a_model_without_restart() {
    let fx = Fixture::start(&[("a", 1.0)], &[]).await;
    assert!(
        !fx.chat("b").await.is_success(),
        "b is not in the registry yet"
    );

    fx.write_registry(&[("a", 1.0), ("b", 1.0)]);

    let mut status = reqwest::StatusCode::NOT_FOUND;
    for _ in 0..50 {
        status = fx.chat("b").await;
        if status.is_success() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        status.is_success(),
        "local/b never served after the edit: {status}"
    );
    assert_eq!(fx.spawn_count("b"), 1);
}

#[tokio::test]
async fn a_small_budget_evicts_the_idle_least_recently_used_model() {
    let fx = Fixture::start(
        &[("a", 4.0), ("b", 4.0), ("c", 4.0)],
        &[("LLM_HUB_LOCAL_MEMORY_GB", "10")],
    )
    .await;
    assert!(fx.chat("a").await.is_success());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fx.chat("b").await.is_success());
    let pid_a = fx.pid_of("a");

    assert!(fx.chat("c").await.is_success());

    let stopped = eventually(Duration::from_secs(5), || !is_alive(pid_a)).await;
    assert!(stopped, "the least recently used model a still runs");
    assert_eq!(fx.spawn_count("b"), 1, "b stays loaded");
}

#[tokio::test]
async fn no_room_when_every_candidate_serves_a_request_answers_503() {
    let fx = Fixture::start(
        &[("slow-a", 4.0), ("slow-b", 4.0), ("c", 4.0)],
        &[
            ("LLM_HUB_LOCAL_MEMORY_GB", "10"),
            ("LLM_HUB_LOCAL_START_TIMEOUT_SECS", "1"),
        ],
    )
    .await;
    // Both slow requests hold their model for 3 s, so no model is idle when `c` starts.
    let busy: Vec<_> = ["slow-a", "slow-b"]
        .into_iter()
        .map(|id| {
            let client = fx.client.clone();
            let url = fx.url("/v1/chat/completions");
            tokio::spawn(async move {
                let body = serde_json::json!({
                    "model": format!("local/{id}"),
                    "messages": [{"role": "user", "content": "hi"}],
                });
                client.post(url).json(&body).send().await.ok()
            })
        })
        .collect();
    let both_started = eventually(Duration::from_secs(10), || {
        fx.spawn_count("slow-a") == 1 && fx.spawn_count("slow-b") == 1
    })
    .await;
    assert!(both_started, "{:?}", fx.spawns());

    let status = fx.chat("c").await;

    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fx.spawn_count("c"), 0);
    for task in busy {
        task.abort();
    }
}

#[tokio::test]
async fn idle_secs_one_stops_the_model_after_its_last_request() {
    let fx = Fixture::start(&[("a", 1.0)], &[("LLM_HUB_LOCAL_IDLE_SECS", "1")]).await;
    assert!(fx.chat("a").await.is_success());
    let pid = fx.pid_of("a");
    assert!(is_alive(pid));

    let stopped = eventually(Duration::from_secs(15), || !is_alive(pid)).await;

    assert!(stopped, "the idle model still runs");
}

#[tokio::test]
async fn sigterm_to_the_hub_stops_every_child() {
    let mut fx = Fixture::start(&[("a", 1.0), ("b", 1.0)], &[]).await;
    assert!(fx.chat("a").await.is_success());
    assert!(fx.chat("b").await.is_success());
    let pids = fx.spawned_pids();
    assert_eq!(pids.len(), 2, "{pids:?}");

    fx.stop_hub();

    for pid in pids {
        assert!(
            eventually(Duration::from_secs(10), || !is_alive(pid)).await,
            "child {pid} survived the hub"
        );
    }
}

#[tokio::test]
async fn default_config_lists_no_local_models_and_starts_none() {
    let fx = Fixture::start_default().await;

    let models = fx.models().await;

    assert!(
        models.iter().all(|id| !id.starts_with("local/")),
        "{models:?}"
    );
    assert!(fx.spawns().is_empty());
}

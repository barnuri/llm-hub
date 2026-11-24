//! Integration tests for `mistralrs` rows of the local serving pool.
//!
//! Default build: a `mistralrs` row answers the feature error, an unsupported flag names the
//! flag, and `llm-hub local-worker` exits non-zero. Feature build: the gate refusal and its two
//! overrides, plus one `#[ignore]` smoke test against a real GGUF.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

use crate::hub;

use std::collections::HashMap;
use std::path::Path;
#[cfg(feature = "mistralrs")]
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use hub::{HubProcess, start_hub};

/// The worker's feature message, as `worker.rs` prints it.
const NOT_BUILT: &str = "this llm-hub was built without the `mistralrs` cargo feature";
/// The gate refusal prefix, as `command.rs` builds it.
#[cfg(feature = "mistralrs")]
const REFUSED: &str = "the mistralrs engine refuses to start";
/// The model file of the smoke test when `LLM_HUB_TEST_GGUF` is unset.
#[cfg(feature = "mistralrs")]
const DEFAULT_SMOKE_GGUF: &str = "/Users/barnu/models/ornith-1.5-9b.gguf";

/// Owns one test's registry, model directory and hub. Dropping it stops the hub.
struct Fixture {
    _dir: tempfile::TempDir,
    _models: Option<tempfile::TempDir>,
    _hub: HubProcess,
    base: String,
}

impl Fixture {
    /// Starts a hub whose `local` profile serves `row` (the registry line, `|`-separated) from
    /// `models_dir`, or from an empty fixture directory with a fake `m.gguf`. `extra` sets vars.
    async fn start(row: &str, models_dir: Option<&Path>, extra: &[(&str, &str)]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let models = models_dir.map_or_else(
            || {
                let path = dir.path().join("models");
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("m.gguf"), "not a model").unwrap();
                path
            },
            Path::to_path_buf,
        );
        std::fs::write(
            dir.path().join("registry.txt"),
            format!("# id|repo|file|size_gb|ctx|quant|extra_flags|bin|engine\n{row}\n"),
        )
        .unwrap();
        let mut vars = HashMap::from([
            ("LLM_HUB_PROFILES".to_owned(), "local".to_owned()),
            ("LLM_HUB_LOCAL_KIND".to_owned(), "local".to_owned()),
            (
                "LLM_HUB_LOCAL_REGISTRY".to_owned(),
                path_str(&dir.path().join("registry.txt")),
            ),
            ("LLM_HUB_LOCAL_MODELS_DIR".to_owned(), path_str(&models)),
            (
                "LLM_HUB_LOCAL_HF_CACHE".to_owned(),
                path_str(&dir.path().join("hf")),
            ),
            (
                "LLM_HUB_LOCAL_GATE_FILE".to_owned(),
                path_str(&dir.path().join("gates.jsonl")),
            ),
            (
                "LLM_HUB_LOCAL_START_TIMEOUT_SECS".to_owned(),
                "20".to_owned(),
            ),
        ]);
        for (key, value) in extra {
            vars.insert((*key).to_owned(), (*value).to_owned());
        }
        let (hub, base) = start_hub(vars).await;
        Fixture {
            _dir: dir,
            _models: None,
            _hub: hub,
            base,
        }
    }

    /// Sends one chat request for `local/m` and returns the status and the body text.
    async fn chat(&self, body: serde_json::Value) -> (reqwest::StatusCode, String) {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_mins(3))
            .build()
            .unwrap();
        let response = client
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    }
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn chat_body(text: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "local/m",
        "messages": [{"role": "user", "content": text}],
    })
}

/// A `mistralrs` registry row for id `m` with the given extra flags.
fn mistral_row(extra_flags: &str) -> String {
    format!("m|org/m|m.gguf|0.1|4096|Q4_K_M|{extra_flags}||mistralrs")
}

#[tokio::test]
async fn mistralrs_row_without_the_feature_answers_the_feature_error() {
    if cfg!(feature = "mistralrs") {
        return;
    }
    let fixture = Fixture::start(&mistral_row(""), None, &[]).await;
    let (status, body) = fixture.chat(chat_body("hi")).await;
    assert!(status.is_server_error(), "{status}: {body}");
    assert!(body.contains(NOT_BUILT), "{body}");
}

#[tokio::test]
async fn mistralrs_row_with_an_unsupported_flag_answers_that_flag() {
    if cfg!(feature = "mistralrs") {
        return;
    }
    let fixture = Fixture::start(&mistral_row("--spec-type ngram-cache"), None, &[]).await;
    let (status, body) = fixture.chat(chat_body("hi")).await;
    assert!(status.is_server_error(), "{status}: {body}");
    assert!(
        body.contains("--spec-type is not supported"),
        "the flag error must come before the feature error: {body}"
    );
}

#[test]
fn local_worker_exits_non_zero_with_the_feature_message() {
    if cfg!(feature = "mistralrs") {
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_llm-hub"))
        .args([
            "local-worker",
            "--id",
            "m",
            "--model",
            "/models/m.gguf",
            "--port",
            "1",
            "--ctx-size",
            "8",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(NOT_BUILT), "{stderr}");
}

/// The gate refusal only runs in the feature build: `build_argv` checks the feature first.
#[cfg(feature = "mistralrs")]
mod gate {
    use super::*;

    #[tokio::test]
    async fn ungated_mistralrs_row_is_refused_without_an_override() {
        let fixture = Fixture::start(&mistral_row(""), None, &[]).await;
        let (status, body) = fixture.chat(chat_body("hi")).await;
        assert!(status.is_server_error(), "{status}: {body}");
        assert!(body.contains(REFUSED), "{body}");
    }

    #[tokio::test]
    async fn ungated_models_override_lets_the_row_past_the_gate() {
        let fixture = Fixture::start(
            &mistral_row(""),
            None,
            &[("LLM_HUB_LOCAL_UNGATED_MODELS", "m")],
        )
        .await;
        let (_, body) = fixture.chat(chat_body("hi")).await;
        assert!(!body.contains(REFUSED), "{body}");
    }

    #[tokio::test]
    async fn allow_ungated_override_lets_the_row_past_the_gate() {
        let fixture = Fixture::start(
            &mistral_row(""),
            None,
            &[("LLM_HUB_LOCAL_ALLOW_UNGATED", "true")],
        )
        .await;
        let (_, body) = fixture.chat(chat_body("hi")).await;
        assert!(!body.contains(REFUSED), "{body}");
    }
}

/// The GGUF of the smoke test: `LLM_HUB_TEST_GGUF`, else the default path.
#[cfg(feature = "mistralrs")]
fn smoke_gguf() -> PathBuf {
    std::env::var_os("LLM_HUB_TEST_GGUF")
        .map_or_else(|| PathBuf::from(DEFAULT_SMOKE_GGUF), PathBuf::from)
}

/// Starts a hub that serves the smoke GGUF as `ornith-1.5-9b`, ungated by id.
///
/// The resolver looks for `<models_dir>/<id>.gguf` first, so the GGUF is linked under the id
/// in a temp models directory. The directory lives as long as the returned fixture.
#[cfg(feature = "mistralrs")]
async fn smoke_fixture() -> Fixture {
    let gguf = smoke_gguf();
    let file = gguf.file_name().unwrap().to_string_lossy().into_owned();
    let models = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&gguf, models.path().join("ornith-1.5-9b.gguf")).unwrap();
    let row = format!("ornith-1.5-9b|org/ornith|{file}|6|8192|Q4_K_M|--reasoning off||mistralrs");
    let mut fixture = Fixture::start(
        &row,
        Some(models.path()),
        &[
            ("LLM_HUB_LOCAL_UNGATED_MODELS", "ornith-1.5-9b"),
            ("LLM_HUB_LOCAL_START_TIMEOUT_SECS", "240"),
        ],
    )
    .await;
    fixture._models = Some(models);
    fixture
}

/// Needs the GGUF and a build with the feature. Run with `--ignored`.
#[cfg(feature = "mistralrs")]
#[tokio::test]
#[ignore = "loads a real GGUF; run with --features mistralrs -- --ignored"]
async fn smoke_streams_a_reply_and_calls_a_tool() {
    let fixture = smoke_fixture().await;
    let streamed = serde_json::json!({
        "model": "local/ornith-1.5-9b",
        "stream": true,
        "messages": [{"role": "user", "content": "Reply with the word ok."}],
    });
    let (status, body) = fixture.chat(streamed).await;
    assert!(status.is_success(), "{status}: {body}");
    assert!(body.contains("data:"), "no streamed chunk: {body}");

    let tool = serde_json::json!({
        "model": "local/ornith-1.5-9b",
        "messages": [{"role": "user", "content": "What is the weather in Paris? Use the tool."}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Gets the weather of a city",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                },
            },
        }],
    });
    let (status, body) = fixture.chat(tool).await;
    assert!(status.is_success(), "{status}: {body}");
    assert!(body.contains("tool_calls"), "no tool call: {body}");
}

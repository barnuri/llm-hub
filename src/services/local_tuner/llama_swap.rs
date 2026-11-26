//! Background reads of llama-swap's /running, /props and /logs, plus the flag parser.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::routes::proxy::{UpstreamEnd, read_upstream_capped};

/// Every read gives up after this long, so a stuck server cannot stall the probe.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The most bytes a read keeps from one response body.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// llama-swap's state for a model that is loaded and serving.
const READY_STATE: &str = "ready";

/// Why a llama-swap read failed.
#[derive(Debug)]
pub enum LlamaSwapError {
    /// The root and path do not form a valid URL.
    Url(String),
    /// The request failed or timed out before a status arrived.
    Http(reqwest::Error),
    /// The server answered with a status that is not a success.
    Status(StatusCode),
    /// The body failed mid-read or grew past [`MAX_BODY_BYTES`].
    Body(UpstreamEnd),
    /// The body is not the expected JSON.
    Json(serde_json::Error),
}

impl LlamaSwapError {
    /// True when the server answered 404, as a plain llama-server does for `/running`.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status(StatusCode::NOT_FOUND))
    }
}

impl fmt::Display for LlamaSwapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(reason) => write!(f, "invalid llama-swap url: {reason}"),
            Self::Http(err) => write!(f, "llama-swap request failed: {err}"),
            Self::Status(status) => write!(f, "llama-swap answered {status}"),
            Self::Body(UpstreamEnd::Open) => {
                write!(f, "llama-swap body is larger than {MAX_BODY_BYTES} bytes")
            }
            Self::Body(_) => write!(f, "llama-swap body read failed"),
            Self::Json(err) => write!(f, "llama-swap body is not the expected json: {err}"),
        }
    }
}

impl std::error::Error for LlamaSwapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::Url(_) | Self::Status(_) | Self::Body(_) => None,
        }
    }
}

/// One model in llama-swap's `GET /running` answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunningModel {
    /// The llama-swap model id.
    pub model: String,
    /// The process state, such as `ready` or `starting`.
    #[serde(default)]
    pub state: String,
    /// The full command llama-swap started the server with.
    #[serde(default)]
    pub cmd: String,
}

impl RunningModel {
    /// True when the model is loaded, so reading its `/upstream/<model>/...` does not load it.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == READY_STATE
    }
}

#[derive(Deserialize)]
struct RunningResponse {
    #[serde(default)]
    running: Vec<RunningModel>,
}

/// The parts of llama-server's `GET /props` answer the tuner reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelProps {
    /// The context size the server runs with, in tokens.
    pub n_ctx: Option<u64>,
}

#[derive(Deserialize)]
struct PropsResponse {
    #[serde(default)]
    default_generation_settings: GenerationSettings,
}

#[derive(Default, Deserialize)]
struct GenerationSettings {
    n_ctx: Option<u64>,
}

/// The flags of a server command line, where the last value of a repeated flag wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveFlags {
    flags: HashMap<String, Option<String>>,
}

impl EffectiveFlags {
    /// Parses `cmd` with shell quoting, or by whitespace when the quoting is unbalanced.
    /// A flag takes the next word as its value unless that word is a flag too; `--flag=value`
    /// also works. Words that belong to no flag, such as the binary, are ignored.
    #[must_use]
    pub fn parse(cmd: &str) -> Self {
        let words = shlex::split(cmd)
            .unwrap_or_else(|| cmd.split_whitespace().map(str::to_string).collect());
        let mut flags = HashMap::new();
        let mut words = words.into_iter().peekable();
        while let Some(word) = words.next() {
            if !is_flag(&word) {
                continue;
            }
            if word.starts_with("--")
                && let Some((name, value)) = word.split_once('=')
            {
                flags.insert(name.to_string(), Some(value.to_string()));
                continue;
            }
            let value = words.next_if(|next| !is_flag(next));
            flags.insert(word, value);
        }
        Self { flags }
    }

    /// The value of `flag`, or `None` when the flag is absent or a bare switch.
    #[must_use]
    pub fn get(&self, flag: &str) -> Option<&str> {
        self.flags.get(flag).and_then(Option::as_deref)
    }

    /// True when the command sets `flag`, with or without a value.
    #[must_use]
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains_key(flag)
    }
}

/// A word is a flag when it starts with `-` and is not a negative number such as `-1`.
fn is_flag(word: &str) -> bool {
    word.len() > 1 && word.starts_with('-') && word.parse::<f64>().is_err()
}

/// The llama-swap root of a profile base URL: trailing `/` and `/v1` removed.
#[must_use]
pub fn root_of(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
}

/// Reads llama-swap's `GET <root>/running`.
///
/// # Errors
/// When the request fails or times out, the status is not a success (404 on a plain
/// llama-server), or the body is not the expected JSON.
pub async fn running(
    client: &reqwest::Client,
    root: &str,
) -> Result<Vec<RunningModel>, LlamaSwapError> {
    let body = fetch(client, endpoint(root, &["running"])?, READ_TIMEOUT).await?;
    let parsed: RunningResponse = parse_json(&body)?;
    Ok(parsed.running)
}

/// Reads `GET <root>/upstream/<model>/props`, or the plain llama-server `GET <root>/props`
/// when `model` is `None`. The llama-swap form loads the model, so call it for ready models only.
///
/// # Errors
/// When the request fails or times out, the status is not a success, or the body is not JSON.
pub async fn props(
    client: &reqwest::Client,
    root: &str,
    model: Option<&str>,
) -> Result<ModelProps, LlamaSwapError> {
    let url = match model {
        Some(model) => endpoint(root, &["upstream", model, "props"])?,
        None => endpoint(root, &["props"])?,
    };
    let body = fetch(client, url, READ_TIMEOUT).await?;
    let parsed: PropsResponse = parse_json(&body)?;
    Ok(ModelProps {
        n_ctx: parsed.default_generation_settings.n_ctx,
    })
}

/// True when the first [`MAX_BODY_BYTES`] of `GET <root>/logs` contain `needle`.
///
/// # Errors
/// When the request fails or times out, the status is not a success, or the body read fails.
pub async fn log_mentions(
    client: &reqwest::Client,
    root: &str,
    needle: &str,
) -> Result<bool, LlamaSwapError> {
    let url = endpoint(root, &["logs"])?;
    let response = send(client, url, READ_TIMEOUT).await?;
    let (body, end) = read_upstream_capped(response, MAX_BODY_BYTES).await;
    if end == UpstreamEnd::Error {
        return Err(LlamaSwapError::Body(end));
    }
    Ok(String::from_utf8_lossy(&body).contains(needle))
}

fn endpoint(root: &str, segments: &[&str]) -> Result<Url, LlamaSwapError> {
    let mut url = Url::parse(root).map_err(|err| LlamaSwapError::Url(err.to_string()))?;
    url.path_segments_mut()
        .map_err(|()| LlamaSwapError::Url(format!("{root} cannot hold a path")))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

async fn send(
    client: &reqwest::Client,
    url: Url,
    timeout: Duration,
) -> Result<reqwest::Response, LlamaSwapError> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(LlamaSwapError::Http)?;
    let status = response.status();
    if !status.is_success() {
        return Err(LlamaSwapError::Status(status));
    }
    Ok(response)
}

async fn fetch(
    client: &reqwest::Client,
    url: Url,
    timeout: Duration,
) -> Result<Vec<u8>, LlamaSwapError> {
    let response = send(client, url, timeout).await?;
    let (body, end) = read_upstream_capped(response, MAX_BODY_BYTES).await;
    if end != UpstreamEnd::Complete {
        return Err(LlamaSwapError::Body(end));
    }
    Ok(body)
}

fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, LlamaSwapError> {
    serde_json::from_slice(body).map_err(LlamaSwapError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const LOADED_CMD: &str = "/opt/llama/llama-server\n  --port 5817\n  --model /m/ornith.gguf\n  --jinja\n  --ctx-size 65536";

    async fn serve(route: &str, response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn running_lists_ready_and_loading_models() {
        let body = serde_json::json!({"running": [
            {"model": "ornith-1.5-9b", "state": "ready", "cmd": LOADED_CMD,
             "proxy": "http://localhost:5817", "ttl": 0, "name": "", "description": ""},
            {"model": "gemma-27b", "state": "starting", "cmd": "llama-server"}
        ]});
        let server = serve("/running", ResponseTemplate::new(200).set_body_json(body)).await;

        let models = running(&crate::services::http_client::client(), &server.uri())
            .await
            .unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].model, "ornith-1.5-9b");
        assert!(models[0].is_ready());
        assert_eq!(models[0].cmd, LOADED_CMD);
        assert!(!models[1].is_ready());
    }

    #[tokio::test]
    async fn running_without_models_is_empty() {
        let server = serve(
            "/running",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        )
        .await;

        let models = running(&crate::services::http_client::client(), &server.uri())
            .await
            .unwrap();

        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn running_on_plain_llama_server_is_not_found() {
        let server = MockServer::start().await;

        let err = running(&crate::services::http_client::client(), &server.uri())
            .await
            .unwrap_err();

        assert!(err.is_not_found());
        assert_eq!(err.to_string(), "llama-swap answered 404 Not Found");
    }

    #[tokio::test]
    async fn running_with_invalid_json_is_a_json_error() {
        let server = serve(
            "/running",
            ResponseTemplate::new(200).set_body_string("nope"),
        )
        .await;

        let err = running(&crate::services::http_client::client(), &server.uri())
            .await
            .unwrap_err();

        assert!(matches!(err, LlamaSwapError::Json(_)));
        assert!(!err.is_not_found());
        assert!(std::error::Error::source(&err).is_some());
    }

    #[tokio::test]
    async fn running_with_oversized_body_is_a_body_error() {
        let huge = format!(
            "{{\"running\":[],\"pad\":\"{}\"}}",
            "x".repeat(MAX_BODY_BYTES)
        );
        let server = serve("/running", ResponseTemplate::new(200).set_body_string(huge)).await;

        let err = running(&crate::services::http_client::client(), &server.uri())
            .await
            .unwrap_err();

        assert!(matches!(err, LlamaSwapError::Body(UpstreamEnd::Open)));
        assert!(err.to_string().contains("larger than"));
    }

    #[tokio::test]
    async fn props_reads_context_through_upstream() {
        let body = serde_json::json!({
            "default_generation_settings": {"n_ctx": 65536, "temperature": 0.8},
            "modalities": {"vision": true, "audio": false},
            "chat_template": "{{ messages }}"
        });
        let server = serve(
            "/upstream/ornith-1.5-9b/props",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;

        let found = props(
            &crate::services::http_client::client(),
            &server.uri(),
            Some("ornith-1.5-9b"),
        )
        .await
        .unwrap();

        assert_eq!(found.n_ctx, Some(65536));
    }

    #[tokio::test]
    async fn props_without_model_reads_plain_llama_server() {
        let server = serve(
            "/props",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        )
        .await;
        let root = format!("{}/", server.uri());

        let found = props(&crate::services::http_client::client(), &root, None)
            .await
            .unwrap();

        assert_eq!(found, ModelProps::default());
    }

    #[tokio::test]
    async fn props_error_status_is_reported() {
        let server = serve("/props", ResponseTemplate::new(503)).await;

        let err = props(&crate::services::http_client::client(), &server.uri(), None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            LlamaSwapError::Status(StatusCode::SERVICE_UNAVAILABLE)
        ));
    }

    #[tokio::test]
    async fn slow_server_times_out() {
        let server = serve(
            "/running",
            ResponseTemplate::new(200).set_delay(Duration::from_millis(500)),
        )
        .await;
        let url = endpoint(&server.uri(), &["running"]).unwrap();

        let err = fetch(
            &crate::services::http_client::client(),
            url,
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();

        let LlamaSwapError::Http(inner) = &err else {
            panic!("expected an http error, got {err:?}");
        };
        assert!(inner.is_timeout());
        assert!(err.to_string().starts_with("llama-swap request failed"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[tokio::test]
    async fn unreachable_server_is_an_http_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let err = running(&crate::services::http_client::client(), &root)
            .await
            .unwrap_err();

        assert!(matches!(err, LlamaSwapError::Http(_)));
    }

    #[tokio::test]
    async fn invalid_root_is_a_url_error() {
        for root in ["not a url", "mailto:someone@example.com"] {
            let err = running(&crate::services::http_client::client(), root)
                .await
                .unwrap_err();

            assert!(matches!(err, LlamaSwapError::Url(_)), "{root}");
            assert!(err.to_string().starts_with("invalid llama-swap url"));
            assert!(std::error::Error::source(&err).is_none());
        }
    }

    #[tokio::test]
    async fn log_mentions_finds_the_needle() {
        let logs = "[INFO] ornith-1.5-9b ready\nW srv    load_model: cache_reuse is not supported by this context, it will be disabled\n";
        let server = serve("/logs", ResponseTemplate::new(200).set_body_string(logs)).await;
        let client = crate::services::http_client::client();

        assert!(
            log_mentions(&client, &server.uri(), "cache_reuse is not supported")
                .await
                .unwrap()
        );
        assert!(
            !log_mentions(&client, &server.uri(), "out of memory")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn log_mentions_reads_only_the_first_mebibyte() {
        let mut logs = "early needle\n".to_string();
        logs.push_str(&"x".repeat(2 * MAX_BODY_BYTES));
        logs.push_str("late needle\n");
        let server = serve("/logs", ResponseTemplate::new(200).set_body_string(logs)).await;
        let client = crate::services::http_client::client();

        assert!(log_mentions(&client, &server.uri(), "early").await.unwrap());
        assert!(!log_mentions(&client, &server.uri(), "late").await.unwrap());
    }

    #[tokio::test]
    async fn log_mentions_reports_error_status() {
        let server = serve("/logs", ResponseTemplate::new(500)).await;

        let err = log_mentions(&crate::services::http_client::client(), &server.uri(), "x")
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            LlamaSwapError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        ));
    }

    #[test]
    fn body_read_failure_message() {
        let err = LlamaSwapError::Body(UpstreamEnd::Error);

        assert_eq!(err.to_string(), "llama-swap body read failed");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn root_of_strips_trailing_v1_and_slashes() {
        assert_eq!(root_of("http://localhost:8080/v1"), "http://localhost:8080");
        assert_eq!(
            root_of("http://localhost:8080/v1/"),
            "http://localhost:8080"
        );
        assert_eq!(root_of("http://localhost:8080/"), "http://localhost:8080");
        assert_eq!(root_of("http://host/swap/v1"), "http://host/swap");
        assert_eq!(root_of("http://host/v12"), "http://host/v12");
    }

    #[test]
    fn endpoint_appends_segments_under_a_root_path() {
        let url = endpoint("http://host/swap/", &["upstream", "a b", "props"]).unwrap();

        assert_eq!(url.as_str(), "http://host/swap/upstream/a%20b/props");
    }

    #[test]
    fn flags_parse_multiline_command() {
        let flags = EffectiveFlags::parse(LOADED_CMD);

        assert_eq!(flags.get("--ctx-size"), Some("65536"));
        assert_eq!(flags.get("--model"), Some("/m/ornith.gguf"));
        assert!(flags.has("--jinja"));
        assert_eq!(flags.get("--jinja"), None);
        assert!(!flags.has("/opt/llama/llama-server"));
        assert!(!flags.has("--cache-reuse"));
    }

    #[test]
    fn flags_parse_line_continuations() {
        let flags = EffectiveFlags::parse("llama-server \\\n  --temp 0.7 \\\n  --flash-attn");

        assert_eq!(flags.get("--temp"), Some("0.7"));
        assert!(flags.has("--flash-attn"));
    }

    #[test]
    fn flags_last_value_wins() {
        let flags = EffectiveFlags::parse(
            "llama-server --temp 0.7 --cache-reuse 256 --temp 1.0 --cache-reuse 0",
        );

        assert_eq!(flags.get("--temp"), Some("1.0"));
        assert_eq!(flags.get("--cache-reuse"), Some("0"));
    }

    #[test]
    fn flags_parse_equals_form_and_short_flags() {
        let flags = EffectiveFlags::parse(
            "llama-server --cache-type-k=q8_0 -c 8192 -fa --seed -1 --alias=a=b --no-mmap",
        );

        assert_eq!(flags.get("--cache-type-k"), Some("q8_0"));
        assert_eq!(flags.get("-c"), Some("8192"));
        assert!(flags.has("-fa"));
        assert_eq!(flags.get("-fa"), None);
        assert_eq!(flags.get("--seed"), Some("-1"));
        assert_eq!(flags.get("--alias"), Some("a=b"));
        assert!(flags.has("--no-mmap"));
    }

    #[test]
    fn flags_parse_quoted_values() {
        let flags = EffectiveFlags::parse(
            r#"llama-server --chat-template-kwargs '{"enable_thinking": false}' --jinja"#,
        );

        assert_eq!(
            flags.get("--chat-template-kwargs"),
            Some(r#"{"enable_thinking": false}"#)
        );
        assert!(flags.has("--jinja"));
    }

    #[test]
    fn flags_parse_unbalanced_quotes_by_whitespace() {
        let flags = EffectiveFlags::parse("llama-server --alias 'open --ctx-size 4096");

        assert_eq!(flags.get("--alias"), Some("'open"));
        assert_eq!(flags.get("--ctx-size"), Some("4096"));
    }

    #[test]
    fn flags_of_empty_command_are_empty() {
        assert_eq!(EffectiveFlags::parse(""), EffectiveFlags::default());
        assert!(!EffectiveFlags::parse("-").has("-"));
    }
}

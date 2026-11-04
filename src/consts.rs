use std::time::Duration;

pub const MODELS_FANOUT_TIMEOUT: Duration = Duration::from_secs(5);
pub const MODELS_CACHE_TTL: Duration = Duration::from_mins(1);
pub const DEFAULT_MAX_REPLAY_BYTES: usize = 256 * 1024 * 1024;
/// The `OpenAI` audio upload limit: multipart bodies are buffered up to this so
/// their `model` field can be rewritten per attempt.
pub const MAX_MULTIPART_BYTES: usize = 25 * 1024 * 1024;
pub const POOL_MAX_IDLE_PER_HOST: usize = 100;
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PORT: u16 = 8410;
pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const ENV_FILE: &str = ".env";
/// Hub log directory, relative to the working directory; managed processes log here too.
pub const LOGS_DIR: &str = "logs";
pub const DEFAULT_MANAGED_IDLE_SECS: u64 = 900;
pub const DEFAULT_MANAGED_START_TIMEOUT_SECS: u64 = 120;
pub const MANAGED_READY_POLL_INTERVAL: Duration = Duration::from_millis(200);
pub const MANAGED_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long a managed process gets to exit after SIGTERM before it is killed.
pub const MANAGED_STOP_GRACE: Duration = Duration::from_secs(5);
pub const STATS_MAX_MODEL_KEYS: usize = 1000;
pub const STATS_OVERFLOW_KEY: &str = "other";
/// Rolling tail kept per response for scraping the `usage` object.
pub const USAGE_SCRAPE_TAIL_BYTES: usize = 64 * 1024;

/// Failed requests are stored with at most this much of the upstream reason.
pub const ERROR_REASON_MAX_CHARS: usize = 500;

/// Statuses at or above this are failures in stats, the store, and the log.
pub const ERROR_STATUS_MIN: u16 = 400;

/// A model needs this many calls in the window before insights rank it.
pub const DEFAULT_INSIGHTS_MIN_REQUESTS: u64 = 5;

/// Window `/api/insights` uses when `range` is omitted.
pub const DEFAULT_INSIGHTS_RANGE: &str = "7d";

/// How long an SSE response may produce no upstream bytes before the hub sends
/// a keepalive comment. A slow local model answers the request headers in
/// milliseconds and then goes silent for the whole prefill (measured: 113s on a
/// ~30k-token prompt), which idle-timeout logic downstream cannot tell apart
/// from a dead connection. 15s sits well inside the usual 30s/60s idle limits.
pub const DEFAULT_SSE_KEEPALIVE_MS: u64 = 15_000;
/// The keepalive itself. A line starting with `:` is a comment in the SSE
/// grammar, so every conformant parser drops it before it reaches the message
/// stream — it is inert in both the `OpenAI` and Anthropic dialects.
pub const SSE_KEEPALIVE_FRAME: &str = ": llm-hub keepalive\n\n";

pub const HEADER_FALLBACKS: &str = "x-llm-hub-fallbacks";
pub const HEADER_FALLBACKS_ALIAS: &str = "x-fallbacks";
pub const HEADER_TIMEOUT_MS: &str = "x-llm-hub-timeout-ms";
pub const HEADER_RETRY_ON: &str = "x-llm-hub-retry-on";
pub const HEADER_SERVED_MODEL: &str = "x-llm-hub-model";
pub const HEADER_ATTEMPTS: &str = "x-llm-hub-attempts";
pub const HEADER_SYSTEM_PROMPT_MODE: &str = "x-llm-hub-system-prompt-mode";
pub const HEADER_REASONING_STRIP: &str = "x-llm-hub-reasoning-strip";
/// Response-side diagnostic: which request transforms actually fired.
pub const HEADER_TRANSFORMS: &str = "x-llm-hub-transforms";
/// Response-side diagnostic on `<profile>/auto`: Auto's own pick, as a qualified id.
pub const HEADER_AUTO_PICK: &str = "x-llm-hub-auto-pick";
/// Response-side diagnostic on `<profile>/auto`: why the served model was chosen.
pub const HEADER_AUTO_REASON: &str = "x-llm-hub-auto-reason";
/// Cap for `x-llm-hub-auto-reason`, in characters; the value is ASCII only.
pub const AUTO_REASON_HEADER_MAX_CHARS: usize = 200;
/// `GET /api/copilot/auto/decisions` returns this many decisions without `limit`.
pub const AUTO_DECISIONS_DEFAULT_LIMIT: usize = 50;
/// The most decisions one `GET /api/copilot/auto/decisions` returns.
pub const AUTO_DECISIONS_MAX_LIMIT: usize = 200;
/// `GET /api/smart-auto/history` returns this many events without `limit`.
pub const SELECTION_HISTORY_DEFAULT_LIMIT: usize = 200;
/// The most events one `GET /api/smart-auto/history` returns.
pub const SELECTION_HISTORY_MAX_LIMIT: usize = 1000;

/// Cap for `x-llm-hub-system-prompt-mode: truncate`, in CHARACTERS (not bytes
/// — see the UTF-8 note in transforms.rs).
pub const SYSTEM_PROMPT_MAX_CHARS: usize = 1000;

/// `OpenAI` and Bedrock Converse both reject function names over 64 bytes, so
/// this one is a BYTE limit — unlike `SYSTEM_PROMPT_MAX_CHARS`, it is what the
/// provider actually enforces.
pub const MAX_TOOL_NAME_LEN: usize = 64;
/// Hex characters of the name digest appended to a truncated tool name.
pub const TOOL_NAME_HASH_LEN: usize = 8;
/// `MAX_TOOL_NAME_LEN` - `TOOL_NAME_HASH_LEN` - 1 (the `_` separator).
pub const TOOL_NAME_PREFIX_LEN: usize = MAX_TOOL_NAME_LEN - TOOL_NAME_HASH_LEN - 1;

/// Every Anthropic Messages request lands on the upstream's chat-completions
/// endpoint regardless of the client-facing path.
pub const ANTHROPIC_UPSTREAM_PATH: &str = "/v1/chat/completions";
pub const ANTHROPIC_MESSAGE_ID_PREFIX: &str = "msg_";
/// Signature attached to every `thinking` block translated from upstream
/// reasoning. Claude Code silently drops a thinking block that has no
/// signature, and there is no real signing key locally, so this only satisfies
/// the shape. It is never forwarded upstream.
pub const ANTHROPIC_THINKING_SIGNATURE: &str = "llm-hub-unsigned";
/// Ceiling for a transform that must buffer a whole (non-SSE) body.
pub const MAX_TRANSFORM_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_hours(24);
/// How often the running process re-reads `.env` for external edits.
pub const ENV_WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// launchd `LaunchAgent` label (and plist file stem) on macOS.
pub const SERVICE_LABEL: &str = "com.barnuri.llm-hub";
/// Port baked into the service definitions as `LLM_HUB_PORT` — process env
/// wins over `.env`, so installed services always serve here.
pub const SERVICE_PORT: u16 = 8888;
/// Bind address baked into the service definitions as `LLM_HUB_BIND`.
pub const SERVICE_BIND: &str = "0.0.0.0";
/// systemd user unit name on Linux / Task Scheduler task name on Windows.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub const SERVICE_NAME: &str = "llm-hub";
pub const GITHUB_REPO: &str = "barnuri/llm-hub";
pub const VERSION: &str = env!("LLM_HUB_VERSION");

/// Claude Code's `[1m]` suffix / native long-context window size.
pub const CONTEXT_TOKENS_1M: u64 = 1_000_000;
/// Suffix Claude Code appends to treat a gateway model id as 1M context.
pub const CONTEXT_1M_SUFFIX: &str = "[1m]";

/// `LLM_HUB_<NAME>_AUTH` value for the direct GitHub OAuth bearer flow.
pub const COPILOT_AUTH_OPENCODE: &str = "github-copilot";
/// `LLM_HUB_<NAME>_AUTH` value for the VS Code Copilot token-exchange flow.
pub const COPILOT_AUTH_VSCODE: &str = "github-copilot-vscode";
/// Public OAuth App client id opencode uses for Copilot device login.
pub const COPILOT_OPENCODE_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
/// Public GitHub App client id of the VS Code Copilot extension.
pub const COPILOT_VSCODE_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const GITHUB_OAUTH_SCOPE: &str = "read:user";
pub const DEFAULT_GITHUB_DOMAIN: &str = "github.com";
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
pub const DEFAULT_GH_PATH: &str = "gh";
/// OS credential store service name for GitHub tokens from `copilot login`.
pub const CREDENTIAL_SERVICE: &str = "llm-hub";
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub const COPILOT_VSCODE_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
pub const COPILOT_VSCODE_API_VERSION: &str = "2025-04-01";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
pub const COPILOT_OPENAI_INTENT: &str = "conversation-edits";
/// A Copilot token is refreshed this long before its `expires_at`.
pub const COPILOT_TOKEN_REFRESH_MARGIN: Duration = Duration::from_mins(5);
/// How long a successful `copilot_internal/user` lookup is reused.
pub const COPILOT_USER_TTL: Duration = Duration::from_hours(1);
/// How long a failed `copilot_internal/user` lookup waits before it is retried.
pub const COPILOT_USER_RETRY: Duration = Duration::from_mins(1);
/// How long a Copilot quota snapshot is served before a refresh. The quota API
/// lags GitHub's billing, so polling faster gains nothing.
pub const COPILOT_USAGE_TTL: Duration = Duration::from_mins(1);
/// GitHub's "Models and pricing for GitHub Copilot" page, served as Markdown.
pub const COPILOT_PRICES_URL: &str = "https://docs.github.com/api/article/body?pathname=/en/copilot/reference/copilot-billing/models-and-pricing";
/// GitHub's "AI model comparison" page, served as Markdown. Its task table describes each model.
pub const COPILOT_DESCRIPTIONS_URL: &str = "https://docs.github.com/api/article/body?pathname=/en/copilot/reference/ai-models/model-comparison";
/// Default for `LLM_HUB_COPILOT_PRICES_REFRESH_SECS`: fetch the Copilot price table daily.
pub const DEFAULT_COPILOT_PRICES_REFRESH_SECS: u64 = 86_400;
/// The shortest time between two fetches, so an unpriced model cannot cause a fetch loop.
pub const COPILOT_PRICES_MIN_GAP: Duration = Duration::from_hours(1);
/// Suffix of a model id advertised with a one-million-token window.
pub const LONG_CONTEXT_SUFFIX: &str = "[1m]";
pub const COPILOT_PRICES_TIMEOUT: Duration = Duration::from_secs(20);
/// Added to the device-flow poll `interval` so polls never arrive early.
pub const COPILOT_DEVICE_POLL_MARGIN: Duration = Duration::from_secs(1);
pub const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

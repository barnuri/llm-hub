use std::time::Duration;

pub const MODELS_FANOUT_TIMEOUT: Duration = Duration::from_secs(5);
pub const MODELS_CACHE_TTL: Duration = Duration::from_mins(1);
pub const DEFAULT_MAX_REPLAY_BYTES: usize = 2 * 1024 * 1024;
pub const POOL_MAX_IDLE_PER_HOST: usize = 100;
pub const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_PORT: u16 = 8410;
pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const ENV_FILE: &str = ".env";
pub const STATS_MAX_MODEL_KEYS: usize = 1000;
pub const STATS_OVERFLOW_KEY: &str = "other";
/// Rolling tail kept per response for scraping the `usage` object.
pub const USAGE_SCRAPE_TAIL_BYTES: usize = 64 * 1024;

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
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Claude Code's `[1m]` suffix / native long-context window size.
pub const CONTEXT_TOKENS_1M: u64 = 1_000_000;
/// Suffix Claude Code appends to treat a gateway model id as 1M context.
pub const CONTEXT_1M_SUFFIX: &str = "[1m]";

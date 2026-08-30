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

pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_hours(24);
/// How often the running process re-reads `.env` for external edits.
pub const ENV_WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// launchd `LaunchAgent` label (and plist file stem) on macOS.
pub const SERVICE_LABEL: &str = "com.barnuri.llm-hub";
/// systemd user unit name on Linux / Task Scheduler task name on Windows.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub const SERVICE_NAME: &str = "llm-hub";
pub const GITHUB_REPO: &str = "barnuri/llm-hub";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

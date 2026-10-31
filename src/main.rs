#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![deny(rust_2018_idioms)]

mod configs;
mod consts;
mod dependencies;
mod routes;
mod schemas;
mod services;
mod utils;

use std::collections::HashMap;

use axum::Router;
use axum::middleware;
use axum::routing::{any, delete, get, post};

use crate::configs::HubConfig;
use crate::consts::{ENV_FILE, LOGS_DIR, VERSION};
use crate::dependencies::state::AppState;
use crate::services::launch;
use crate::services::store::Store;

/// Env-file pairs merged with process env (process env wins). Reads via
/// `from_path_iter` so reloads never mutate global process state.
#[must_use]
pub fn load_env_vars() -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = HashMap::new();
    if let Ok(iter) = dotenvy::from_path_iter(ENV_FILE) {
        for (key, value) in iter.flatten() {
            vars.insert(key, value);
        }
    }
    for (key, value) in std::env::vars() {
        vars.insert(key, value);
    }
    vars
}

#[tokio::main]
async fn main() {
    if run_launch_command().await {
        return;
    }
    let _tracing_guard = init_tracing();

    if run_cli_command().await {
        return;
    }

    let vars = load_env_vars();
    let config = match HubConfig::from_map(&vars) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    let store = match open_store(&config) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("store error: {e}");
            std::process::exit(1);
        }
    };

    let bind = format!("{}:{}", config.bind, config.port);
    let auto_update = config.auto_update;
    let state = match AppState::new(config, store) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("startup error: {e}");
            std::process::exit(1);
        }
    };

    routes::models::restore_catalogs(&state);
    routes::models::spawn_background_refresh(state.clone());
    state.managed().spawn_idle_reaper();
    let managed = std::sync::Arc::clone(state.managed());
    let restart_notify = state.restart_notify();
    services::update::spawn_auto_update(auto_update, restart_notify.clone());
    services::config_reload::spawn_env_watcher(state.clone(), restart_notify.clone());

    let app = build_router(state);

    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("llm-hub v{VERSION} listening on http://{bind}");
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = wait_for_shutdown_signal() => {}
                () = restart_notify.notified() => {}
            }
        })
        .await
    {
        managed.stop_all().await;
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
    managed.stop_all().await;

    match services::restart::respawn_if_requested() {
        Ok(true) => tracing::info!("respawned llm-hub after restart request"),
        Ok(false) => {}
        Err(e) => eprintln!("restart respawn failed: {e}"),
    }
}

/// Console logging always; plus a `./logs/llm-hub.log` file sink when not
/// supervised (supervised runs already get that file via the service's
/// stdout/stderr redirect — a second writer would interleave with it).
/// The returned guard must live for the whole process so buffered lines flush.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("llm_hub=info"));
    let console = tracing_subscriber::fmt::layer();
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(console);

    if !services::restart::is_supervised() {
        if let Err(e) = std::fs::create_dir_all(LOGS_DIR) {
            registry.init();
            tracing::warn!("cannot create {LOGS_DIR} dir, file logging disabled: {e}");
            return None;
        }
        let appender = tracing_appender::rolling::never(LOGS_DIR, "llm-hub.log");
        let (file_writer, guard) = tracing_appender::non_blocking(appender);
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(file_writer),
            )
            .init();
        return Some(guard);
    }
    registry.init();
    None
}

/// Ctrl-C, SIGTERM from a supervisor, or on Windows Ctrl+Break and a closed
/// console window, so managed processes are stopped on the way out.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close};
        if let (Ok(mut ctrl_break), Ok(mut ctrl_close)) = (ctrl_break(), ctrl_close()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = ctrl_break.recv() => {}
                _ = ctrl_close.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

const HELP: &str = "llm-hub — one local endpoint for every LLM provider and coding agent

usage: llm-hub [command] [args...]

Server:
  llm-hub                         run the hub in the foreground (same as `serve`)
  llm-hub serve                   run the hub in the foreground
  llm-hub service install         install or reload the background service (macOS LaunchAgent)
  llm-hub service uninstall       remove the background service
  llm-hub service status          show the background service status
  llm-hub update                  update to the latest version (git pull + build for a source checkout)
  llm-hub version                 print the version (also --version, -V)

Coding agents on the hub ([model] is optional, e.g. copilot/claude-opus-5-5):
  llm-hub claude [model] [args]       Claude Code
  llm-hub codex [model] [args]        Codex CLI
  llm-hub opencode [model] [args]     opencode
  llm-hub copilot-cli [model] [args]  GitHub Copilot CLI
  llm-hub pi [model] [args]           pi
  --profiles a,b                      only list models from those profiles; goes before or
                                      after the agent name (also --profiles=a,b), e.g.
                                      llm-hub --profiles copilot,ivrit claude

GitHub Copilot upstream:
  llm-hub copilot login [--profile NAME]   sign in with the GitHub device flow
  llm-hub copilot logout [--profile NAME]  remove the saved GitHub token

Other:
  llm-hub help                    show this help (also --help, -h)

Environment: LLM_HUB_URL (default http://localhost:8888) and LLM_HUB_API_KEY for the agent commands.";

/// Handles the non-server commands. Returns true when one was handled and the
/// process should exit. An unknown command prints the help and exits with 2
/// rather than starting a second server.
async fn run_cli_command() -> bool {
    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => false,
        Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            true
        }
        Some("--version" | "-V" | "version") => {
            println!("llm-hub v{VERSION}");
            true
        }
        Some("update") => {
            if let Err(e) = services::update::self_update().await {
                eprintln!("update failed: {e}");
                std::process::exit(1);
            }
            true
        }
        Some("service") => {
            run_service_command();
            true
        }
        Some("copilot") => {
            if let Err(e) = run_copilot_command().await {
                eprintln!("copilot: {e}");
                std::process::exit(1);
            }
            true
        }
        Some(unknown) => {
            eprintln!("llm-hub: unknown command '{unknown}'\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

/// `llm-hub [--profiles a,b] <claude|codex|opencode|copilot-cli|pi> [model] [args...]`
/// starts that harness on the hub. It runs before tracing starts, so no `logs/`
/// dir appears in the project the harness opens.
async fn run_launch_command() -> bool {
    let command = match launch::LaunchCommand::parse(std::env::args().skip(1).collect()) {
        Ok(Some(command)) => command,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("llm-hub: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    let harness = command.harness;
    if let Err(e) = launch::run(command).await {
        eprintln!("llm-hub {}: {e}", harness.command());
        std::process::exit(1);
    }
    true
}

const COPILOT_USAGE: &str = "usage: llm-hub copilot <login|logout> [--profile NAME]";

/// `llm-hub copilot login|logout [--profile NAME]`.
async fn run_copilot_command() -> Result<(), String> {
    use std::sync::Arc;

    use crate::services::copilot_login;
    use crate::services::credential_store::CredentialStore;
    use crate::services::keyring_credential_store::KeyringCredentialStore;
    use crate::services::process_gh_cli::ProcessGhCli;

    let args: Vec<String> = std::env::args().skip(2).collect();
    let (action, requested) = match args.as_slice() {
        [action] => (action.as_str(), None),
        [action, flag, name] if flag == "--profile" => (action.as_str(), Some(name.as_str())),
        _ => return Err(COPILOT_USAGE.to_string()),
    };
    if action != "login" && action != "logout" {
        return Err(COPILOT_USAGE.to_string());
    }
    let config = HubConfig::from_map(&load_env_vars()).map_err(|e| format!("config error: {e}"))?;
    let (profile, auth) = copilot_login::select_profile(&config, requested)?;
    let account = auth.flow.credential_account(&auth.github_domain);
    let credentials: Arc<dyn CredentialStore> = Arc::new(KeyringCredentialStore);
    if action == "logout" {
        if copilot_login::logout(auth, credentials).await? {
            println!(
                "Removed the login for profile {profile} (account {account}) from the OS credential store"
            );
        } else {
            println!("No login was stored for profile {profile} (account {account})");
        }
        return Ok(());
    }
    let client = reqwest::Client::builder()
        .user_agent(format!("llm-hub/{VERSION}"))
        .build()
        .map_err(|e| e.to_string())?;
    copilot_login::login(&client, profile, auth, credentials, Arc::new(ProcessGhCli)).await?;
    println!(
        "Stored the login for profile {profile} in the OS credential store (account {account})"
    );
    Ok(())
}

fn run_service_command() {
    let action = std::env::args().nth(2).unwrap_or_default();
    let result = match action.as_str() {
        "install" => services::service_manager::install(),
        "uninstall" => services::service_manager::uninstall(),
        "status" => services::service_manager::status(),
        _ => {
            eprintln!("usage: llm-hub service <install|uninstall|status>");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("service {action} failed: {e}");
        std::process::exit(1);
    }
}

fn build_router(state: AppState) -> Router {
    let proxied = Router::new()
        .route("/v1/messages", post(routes::anthropic::messages))
        .route("/v1/{*path}", any(routes::proxy::proxy))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            routes::models::await_first_catalog,
        ));
    let protected = Router::new()
        .merge(proxied)
        .route("/v1/models", get(routes::models::list_models))
        .route("/v1/models/{*id}", get(routes::proxy::get_model_by_id))
        .route(
            "/api/profiles",
            get(routes::admin::list_profiles).post(routes::admin::upsert_profile),
        )
        .route(
            "/api/profiles/{name}",
            delete(routes::admin::delete_profile),
        )
        .route(
            "/api/profiles/{name}/test",
            post(routes::admin::test_profile),
        )
        .route(
            "/api/claude-code/settings",
            get(routes::claude_code::settings),
        )
        .route(
            "/api/copilot/auto/decisions",
            get(routes::copilot::auto_decisions),
        )
        .route("/api/copilot/usage", get(routes::copilot::usage))
        .route("/api/fallbacks", post(routes::admin::set_default_fallbacks))
        .route("/api/restart", post(routes::admin::restart_server))
        .route("/api/stats", get(routes::admin::stats))
        .route("/api/usage", get(routes::admin::usage))
        .route("/api/errors", get(routes::admin::errors))
        .route("/api/insights", get(routes::admin::insights))
        .route("/api/insights/pick", get(routes::admin::insights_pick))
        .route(
            "/api/insights/report",
            get(routes::admin::insights_agent_report),
        )
        .route(
            "/api/keys",
            get(routes::admin::list_api_keys).post(routes::admin::create_api_key),
        )
        .route("/api/keys/{name}", delete(routes::admin::delete_api_key))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            services::auth::require_master_key,
        ));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .fallback(routes::ui::serve)
        .with_state(state)
}

fn open_store(config: &HubConfig) -> Result<Option<Store>, String> {
    if !config.persistent {
        return Ok(None);
    }
    Store::open(&config.store_kind, config.store_path.as_deref()).map(Some)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::services::copilot_auto::AutoTurn;
    use crate::services::copilot_auto_decisions::AutoDecision;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const MASTER_KEY: &str = "hub-master-key";
    const DISCOVERY_PATH: &str = "/v1/models?limit=1000";

    async fn serve_hub_with_auth() -> String {
        serve_hub_with_auth_state().await.0
    }

    async fn serve_hub_with_auth_state() -> (String, AppState) {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_MASTER_KEY", MASTER_KEY),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        state.set_model_cache(json!({"object": "list", "data": [
            {"id": "copilot/claude-opus-4-5", "owned_by": "copilot"},
        ]}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = build_router(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        (base, state)
    }

    #[tokio::test]
    async fn claude_code_discovery_request_accepts_either_credential_header() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}{DISCOVERY_PATH}");

        let with_api_key = client
            .get(&url)
            .header("x-api-key", MASTER_KEY)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .unwrap();
        let with_bearer = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_api_key.status().as_u16(), 200);
        let body: serde_json::Value = with_api_key.json().await.unwrap();
        assert_eq!(body["data"][0]["id"], "copilot/claude-opus-4-5");
        assert_eq!(with_bearer.status().as_u16(), 200);
        assert_eq!(without_key.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn claude_code_settings_route_is_protected() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/claude-code/settings");

        let with_key = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 200);
        let body: serde_json::Value = with_key.json().await.unwrap();
        assert_eq!(
            body["modelPicker"]["options"][0]["model"],
            "copilot/claude-opus-4-5"
        );
        assert_eq!(without_key.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn copilot_usage_route_is_protected() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/copilot/usage?profile=copilot");

        let with_key = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 404);
        assert_eq!(without_key.status().as_u16(), 401);
    }

    fn auto_decision(ts_ms: u64) -> AutoDecision {
        AutoDecision {
            ts_ms,
            session: Some("session-1".to_string()),
            profile: "copilot".to_string(),
            auto_pick: "copilot/claude-haiku-5-5".to_string(),
            model: "copilot/claude-haiku-5-5".to_string(),
            swap_reason: None,
            complexity: Some("low"),
            selection_reason: None,
            turn: AutoTurn::Routed,
            estimated_prompt_tokens: 10,
        }
    }

    async fn decision_count(client: &reqwest::Client, url: &str) -> usize {
        let response = client
            .get(url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        body["decisions"].as_array().unwrap().len()
    }

    #[tokio::test]
    async fn copilot_auto_decisions_route_is_protected() {
        let (base, state) = serve_hub_with_auth_state().await;
        state.copilot_auto().decisions().record(auto_decision(1));
        let client = reqwest::Client::new();
        let url = format!("{base}/api/copilot/auto/decisions?session=session-1&limit=1");

        let with_key = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 200);
        let body: serde_json::Value = with_key.json().await.unwrap();
        assert_eq!(body["decisions"][0]["session"], "session-1");
        assert_eq!(body["decisions"][0]["turn"], "routed");
        assert_eq!(without_key.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn copilot_auto_decisions_limit_is_clamped() {
        let (base, state) = serve_hub_with_auth_state().await;
        for ts_ms in 0..250 {
            state
                .copilot_auto()
                .decisions()
                .record(auto_decision(ts_ms));
        }
        let client = reqwest::Client::new();
        let url = |query: &str| format!("{base}/api/copilot/auto/decisions{query}");

        assert_eq!(decision_count(&client, &url("")).await, 50);
        assert_eq!(decision_count(&client, &url("?limit=0")).await, 1);
        assert_eq!(decision_count(&client, &url("?limit=1000")).await, 200);
        assert_eq!(decision_count(&client, &url("?limit=7")).await, 7);
    }
}

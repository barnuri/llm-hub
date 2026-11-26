//! llm-hub: one local endpoint for every LLM provider and coding agent. Use [`Hub`] to run it
//! with your own [`ext::Extension`]s, or [`run`] for the stock binary.
#![warn(missing_docs, missing_debug_implementations)]

mod configs;
mod consts;
mod dependencies;
pub mod ext;
mod hub;
mod hub_runtime;
mod routes;
mod schemas;
mod services;
mod utils;

use std::collections::HashMap;

use axum::Router;
use axum::middleware;
use axum::routing::{any, get};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

pub use crate::hub::Hub;
pub use crate::hub_runtime::HubRuntime;
pub use crate::services::local_serving::watchdog::{StreamWatchdog, stall_event};
/// The `axum` and `serde_json` versions llm-hub's API uses, so extensions match them.
pub use {axum, serde_json};

use crate::configs::HubConfig;
use crate::consts::{LOGS_DIR, VERSION};
use crate::dependencies::state::AppState;
use crate::services::extension_loader::{self, EXTENSIONS_FILE_VAR, ExtensionSources};
use crate::services::launch;
use crate::services::store::Store;

/// Env-file pairs merged with process env (process env wins). Reads via
/// `from_path_iter` so reloads never mutate global process state.
#[must_use]
pub(crate) fn load_env_vars() -> HashMap<String, String> {
    HubRuntime::process().load_vars()
}

/// Runs a CLI command, ensures an installed service is up, or starts the server.
pub async fn run() {
    if std::env::args().nth(1).is_none() && !services::restart::is_supervised() {
        match services::service_manager::ensure_installed_service().await {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                eprintln!("service check failed: {error}");
                std::process::exit(1);
            }
        }
    }
    Hub::new().run().await;
}

async fn serve(hub: Hub) {
    if run_smart_auto_command().await {
        return;
    }
    if run_local_worker_command().await {
        return;
    }
    if run_launch_command().await {
        return;
    }
    let _tracing_guard = init_tracing();

    if run_cli_command().await {
        return;
    }

    if let Some(bind) = configured_bind(&HubRuntime::process()) {
        services::instance::set_bind(&bind);
        if services::instance::handover_from().is_none()
            && let Some(pid) = services::instance::live_server(&bind)
        {
            if services::restart::is_supervised() {
                // The supervisor restarts this process after the live one exits.
                services::instance::wait_while_served(&bind).await;
                return;
            }
            eprintln!("llm-hub is already running (pid {pid}) on {bind}");
            std::process::exit(1);
        }
    }

    let built = match build_server(hub, HubRuntime::process()) {
        Ok(built) => built,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let Built {
        app,
        bind,
        vars,
        managed,
        local_pool,
        restart_notify,
    } = built;

    let listener = match services::instance::bind_reuseport(&bind).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot bind {bind}: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = services::instance::write_pid(&bind) {
        eprintln!("cannot write the pid file for {bind}: {e}");
    }
    tracing::info!("llm-hub v{VERSION} listening on http://{bind}");
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = wait_for_shutdown_signal() => {}
                () = restart_notify.notified() => {}
                () = services::instance::parent_exit(&vars) => {}
            }
        })
        .await
    {
        managed.stop_all().await;
        local_pool.stop_all().await;
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
    managed.stop_all().await;
    local_pool.stop_all().await;

    match services::restart::respawn_if_requested() {
        Ok(true) => tracing::info!("respawned llm-hub after restart request"),
        Ok(false) => {}
        Err(e) => eprintln!("restart respawn failed: {e}"),
    }
}

/// A configured hub that is ready to serve: its router, the address it binds and the handles that
/// stop its processes.
struct Built {
    app: Router,
    bind: String,
    vars: HashMap<String, String>,
    managed: std::sync::Arc<services::managed_process::ProcessManager>,
    local_pool: std::sync::Arc<services::local_serving::pool::LocalPool>,
    restart_notify: std::sync::Arc<tokio::sync::Notify>,
}

/// Reads the settings, starts the background loops and builds the router. It never exits the
/// process: every failure comes back as the message to print.
fn build_server(hub: Hub, runtime: HubRuntime) -> Result<Built, String> {
    let vars = runtime.load_vars();
    let (config, profile_errors) = HubConfig::from_map_skipping_invalid_profiles(&vars)
        .map_err(|e| format!("config error: {e}"))?;
    for error in profile_errors {
        tracing::warn!(%error, "skipping profile with invalid configuration");
    }
    let store = open_store(&config).map_err(|e| format!("store error: {e}"))?;

    let bind = format!("{}:{}", config.bind, config.port);
    let auto_update = config.auto_update;
    let state =
        AppState::new_in(config, store, runtime).map_err(|e| format!("startup error: {e}"))?;

    routes::models::restore_catalogs(&state);
    routes::models::spawn_background_refresh(state.clone());
    services::copilot_prices::CopilotPrices::spawn_refresh_loop(state.clone());
    services::keep_warm::KeepWarm::spawn_refresh_loop(state.clone());
    services::quota_snapshots::QuotaSnapshots::spawn_poll_loop(state.clone());
    spawn_notifications(&state);
    services::local_tuner::probe::spawn_probe_loop(state.clone());
    state.managed().spawn_idle_reaper();
    let local_pool_boot = std::sync::Arc::clone(state.local_pool());
    match services::instance::handover_from() {
        // The process being replaced still owns its local model processes. Start ours after it exits.
        Some(old) => {
            tokio::spawn(async move {
                services::instance::wait_gone(old).await;
                local_pool_boot.spawn_background();
            });
        }
        None => local_pool_boot.spawn_background(),
    }
    let managed = std::sync::Arc::clone(state.managed());
    let local_pool = std::sync::Arc::clone(state.local_pool());
    let restart_notify = state.restart_notify();
    services::update::spawn_auto_update(auto_update, std::sync::Arc::clone(&restart_notify));
    services::config_reload::spawn_env_watcher(
        state.clone(),
        std::sync::Arc::clone(&restart_notify),
    );
    install_extensions(&state, hub.extensions, &vars)?;

    let mut app = build_router_with(state, hub.routes);
    for change in hub.router_changes {
        app = change(app);
    }
    Ok(Built {
        app,
        bind,
        vars,
        managed,
        local_pool,
        restart_notify,
    })
}

/// The address `runtime` serves, or `None` when its config does not load (startup reports that).
fn configured_bind(runtime: &HubRuntime) -> Option<String> {
    let vars = runtime.load_vars();
    let (config, _) = HubConfig::from_map_skipping_invalid_profiles(&vars).ok()?;
    Some(format!("{}:{}", config.bind, config.port))
}

/// Serves `hub` on `listener` until `shutdown` resolves, then stops its processes. Unlike
/// [`Hub::run`], it takes its paths and variables from `runtime`, installs no log subscriber,
/// handles no signal and never exits the process, so tests can run many hubs in one process.
async fn serve_on(
    hub: Hub,
    runtime: HubRuntime,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let Built {
        app,
        managed,
        local_pool,
        restart_notify,
        ..
    } = build_server(hub, runtime)?;
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = shutdown => {}
                () = restart_notify.notified() => {}
            }
        })
        .await;
    managed.stop_all().await;
    local_pool.stop_all().await;
    served.map_err(|e| format!("server error: {e}"))
}

/// The install setting that remembers the user saw the desktop test popup.
const DESKTOP_CONFIRMED_KEY: &str = "desktop_notify_confirmed";

/// Starts notification delivery to the configured sinks, the snooze prune loop and the budget loop.
fn spawn_notifications(state: &AppState) {
    let config = state.config();
    let notify = &config.notify;
    let confirmed = state
        .store()
        .and_then(|store| store.install_setting(DESKTOP_CONFIRMED_KEY).ok().flatten())
        .is_some_and(|value| value == "1");
    let hook_state = state.clone();
    let hook = services::notifications::desktop::ConfirmHook::new(move || {
        let Some(store) = hook_state.store() else {
            return;
        };
        if let Err(e) = store.set_install_setting(DESKTOP_CONFIRMED_KEY, "1") {
            tracing::warn!("cannot save the desktop popup confirmation: {e}");
        }
    });
    let desktop = std::sync::Arc::new(
        services::notifications::desktop::DesktopSink::new(notify.desktop, notify.timeout)
            .with_confirmation(confirmed, Some(hook)),
    );
    state
        .notifier()
        .set_desktop(std::sync::Arc::clone(&desktop));
    let mut sinks: Vec<Box<dyn services::notifications::dispatch::Sink>> = vec![Box::new(desktop)];
    if let Some(url) = notify.webhook_url.clone() {
        match reqwest::Client::builder().build() {
            Ok(client) => sinks.push(Box::new(
                services::notifications::webhook::WebhookSink::new(client, url, notify.timeout),
            )),
            Err(e) => tracing::warn!("notification webhook disabled: {e}"),
        }
    }
    if let Some(argv) = notify.command.clone() {
        sinks.push(Box::new(
            services::notifications::command_sink::CommandSink::new(argv, notify.timeout),
        ));
    }
    state.notifier().spawn_delivery(sinks);
    state
        .notifier()
        .spawn_prune_loop(std::time::Duration::from_mins(1));
    services::notifications::budget_loop::spawn_budget_loop(state.clone());
    services::notifications::cache_loop::spawn_cache_loop(state.clone());
}

/// Loads the rules file, sets the running extensions and starts the rules watcher.
///
/// # Errors
/// The message to print when the rules file is invalid, which stops startup.
fn install_extensions(
    state: &AppState,
    library: Vec<std::sync::Arc<dyn ext::Extension>>,
    vars: &HashMap<String, String>,
) -> Result<(), String> {
    let runtime = std::sync::Arc::new(state.runtime().clone());
    let sources = std::sync::Arc::new(ExtensionSources::new(
        library,
        runtime.rules_dir().to_path_buf(),
    ));
    let rules = sources
        .load_rules(vars)
        .map_err(|e| format!("extensions error: {e}"))?;
    if let Some((path, rules)) = &rules {
        tracing::info!(
            "loaded {} extension rules from {}",
            rules.len(),
            path.display()
        );
    }
    state.set_extensions(sources.compose(rules.map(|(_, rules)| rules)));
    extension_loader::spawn_rules_watcher(
        state.clone(),
        sources,
        std::sync::Arc::new(move || runtime.load_vars()),
    );
    Ok(())
}

/// The directory llm-hub looks in for a default rules file: the working directory.
fn rules_dir() -> std::path::PathBuf {
    HubRuntime::process().rules_dir().to_path_buf()
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
  llm-hub                         ensure the installed service is up, or run in the foreground
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

Extensions:
  llm-hub extensions check [FILE]  validate a rules file (default: LLM_HUB_EXTENSIONS_FILE or extensions.yaml)

GitHub Copilot upstream:
  llm-hub copilot login [--profile NAME]   sign in with the GitHub device flow
  llm-hub copilot logout [--profile NAME]  remove the saved GitHub token

Other:
  llm-hub smart-auto export --workflow <id> --out <path>
                                  write a workflow's Smart Auto history into the generated
                                  block of a Markdown file (git-tracked files are refused)
  llm-hub local-worker --id <id> --model <gguf> --port <port> [flags]
                                  serve one local model with mistral.rs (needs the `mistralrs`
                                  build); the hub starts it for `mistralrs` registry rows
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
        Some("extensions") => {
            if let Err(e) = run_extensions_command() {
                eprintln!("extensions: {e}");
                std::process::exit(1);
            }
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

/// `llm-hub local-worker --id … --model … --port … [mapped flags]` serves one local model. It runs
/// before tracing starts. Returns true when the command was handled.
async fn run_local_worker_command() -> bool {
    if std::env::args().nth(1).as_deref() != Some("local-worker") {
        return false;
    }
    let words: Vec<String> = std::env::args().skip(2).collect();
    if let Err(message) = services::local_serving::worker::run(&words).await {
        eprintln!("llm-hub local-worker: {message}");
        std::process::exit(1);
    }
    true
}

/// `llm-hub smart-auto export --workflow <id> --out <path>`. It runs before tracing starts, so no
/// `logs/` dir appears in the working directory. Returns true when the command was handled.
async fn run_smart_auto_command() -> bool {
    if std::env::args().nth(1).as_deref() != Some("smart-auto") {
        return false;
    }
    let args: Vec<String> = std::env::args().skip(2).collect();
    match services::history_exporter::run(&args).await {
        Ok(message) => println!("{message}"),
        Err(e) => {
            eprintln!("llm-hub smart-auto: {e}");
            std::process::exit(1);
        }
    }
    true
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

const EXTENSIONS_USAGE: &str = "usage: llm-hub extensions check [FILE]";

/// `llm-hub extensions check [FILE]`: loads a rules file the way the server does and reports it.
fn run_extensions_command() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut vars = load_env_vars();
    match args.as_slice() {
        [check] if check == "check" => {}
        [check, file] if check == "check" => {
            vars.insert(EXTENSIONS_FILE_VAR.to_owned(), file.clone());
        }
        _ => return Err(EXTENSIONS_USAGE.to_owned()),
    }
    let sources = ExtensionSources::new(Vec::new(), rules_dir());
    match sources.load_rules(&vars)? {
        Some((path, rules)) => {
            let (hooks, shadow_hooks) = rules.hook_counts();
            println!(
                "{}: {} rules ({} in shadow mode), {hooks} hooks ({shadow_hooks} in shadow mode), valid",
                path.display(),
                rules.len(),
                rules.shadow_count()
            );
        }
        None => println!("no rules file: set {EXTENSIONS_FILE_VAR} or add extensions.yaml"),
    }
    Ok(())
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

/// Every documented route, collected with its `#[utoipa::path]`. A route added here without
/// that attribute does not compile. Raw `.route(...)` calls (wildcards) are checked by a test.
fn api_router(state: &AppState) -> OpenApiRouter<AppState> {
    use routes::smart_auto;
    use routes::{
        admin, anthropic, cache, claude_code, copilot, live, models, profile_usage, sessions,
    };

    let proxied = OpenApiRouter::new()
        .routes(routes!(anthropic::messages))
        .route("/v1/{*path}", any(routes::proxy::proxy))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            routes::models::await_first_catalog,
        ));
    OpenApiRouter::with_openapi(routes::api_docs::ApiDoc::openapi())
        .merge(proxied)
        .routes(routes!(models::list_models))
        .route("/v1/models/{*id}", get(routes::proxy::get_model_by_id))
        .routes(routes!(admin::list_profiles, admin::upsert_profile))
        .routes(routes!(admin::set_local_model_engine))
        .routes(routes!(admin::delete_profile))
        .routes(routes!(admin::test_profile))
        .routes(routes!(claude_code::settings))
        .routes(routes!(copilot::auto_decisions))
        .routes(routes!(smart_auto::current))
        .routes(routes!(smart_auto::get_rules, smart_auto::put_rules))
        .routes(routes!(smart_auto::rules_effect))
        .routes(routes!(smart_auto::history))
        .routes(routes!(smart_auto::report_outcome))
        .routes(routes!(smart_auto::attempt_text))
        .routes(routes!(smart_auto::decisions))
        .routes(routes!(smart_auto::health))
        .routes(routes!(smart_auto::resolve))
        .routes(routes!(smart_auto::tiers))
        .routes(routes!(copilot::usage))
        .routes(routes!(profile_usage::sources))
        .routes(routes!(profile_usage::usage))
        .routes(routes!(copilot::usage_history))
        .routes(routes!(admin::set_default_fallbacks))
        .routes(routes!(admin::set_copilot_prices))
        .routes(routes!(admin::set_keep_warm))
        .routes(routes!(admin::set_media_fallback))
        .routes(routes!(admin::restart_server))
        .routes(routes!(routes::extensions::list))
        .routes(routes!(routes::extensions::set_extension))
        .routes(routes!(routes::extensions::set_rule))
        .routes(routes!(admin::stats))
        .routes(routes!(cache::list))
        .routes(routes!(admin::usage))
        .routes(routes!(live::stream))
        .routes(routes!(admin::errors))
        .routes(routes!(admin::resolve_errors))
        .routes(routes!(admin::resolve_error_group))
        .routes(routes!(sessions::list))
        .routes(routes!(sessions::detail))
        .routes(routes!(admin::insights))
        .routes(routes!(admin::insights_pick))
        .routes(routes!(admin::insights_agent_report))
        .routes(routes!(admin::insights_analysis_models))
        .routes(routes!(admin::insights_analyze))
        .routes(routes!(admin::list_api_keys, admin::create_api_key))
        .routes(routes!(admin::delete_api_key))
        .routes(routes!(
            routes::notifications::list,
            routes::notifications::ingest
        ))
        .routes(routes!(routes::notifications::approve))
        .routes(routes!(routes::notifications::dismiss))
        .routes(routes!(
            routes::notifications::list_mutes,
            routes::notifications::add_mute
        ))
        .routes(routes!(routes::notifications::delete_mute))
        .routes(routes!(routes::notifications::desktop_status))
        .routes(routes!(routes::notifications::desktop_test))
        .routes(routes!(routes::notifications::desktop_confirm))
        .routes(routes!(routes::notifications::desktop_fix))
}

#[cfg(test)]
fn build_router(state: AppState) -> Router {
    build_router_with(state, None)
}

/// The hub's routes, plus `ext_routes` under `/ext`. Both sit behind the master key.
fn build_router_with(state: AppState, ext_routes: Option<Router>) -> Router {
    let (protected, openapi) = api_router(&state).split_for_parts();
    // The spec and the docs UI sit under the same key check as /api/*.
    let mut protected = protected.merge(SwaggerUi::new("/api-docs").url("/openapi.json", openapi));
    if let Some(ext_routes) = ext_routes {
        protected = protected.nest_service("/ext", ext_routes);
    }
    let protected = protected.layer(middleware::from_fn_with_state(
        state.clone(),
        services::auth::require_master_key,
    ));
    let protected = protected.layer(middleware::from_fn(services::restart::track_inference));

    let (health, _) = OpenApiRouter::<AppState>::new()
        .routes(routes!(routes::admin::healthz))
        .routes(routes!(routes::admin::build_info))
        .split_for_parts();
    let phases_state = state.clone();
    Router::new()
        .merge(health)
        .merge(protected)
        .fallback(routes::ui::serve)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            phases_state,
            services::extension_http::run_phases,
        ))
}

fn open_store(config: &HubConfig) -> Result<Option<Store>, String> {
    if !config.persistent {
        return Ok(None);
    }
    Store::open(&config.store_kind, config.store_path.as_deref()).map(Some)
}

#[cfg(test)]
mod tests {
    #[tokio::test(flavor = "multi_thread")]
    async fn zz_profile_startup_costs() {
        use std::time::Instant;
        let mut out = String::new();
        let n = 20;
        macro_rules! t {
            ($name:expr, $body:expr) => {{
                let start = Instant::now();
                for _ in 0..n {
                    let _ = std::hint::black_box($body);
                }
                out.push_str(&format!("{}: {:.1} ms\n", $name, start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)));
            }};
        }
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "alpha,beta"),
            ("LLM_HUB_ALPHA_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_BETA_BASE_URL", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        t!("load_native_certs", rustls_native_certs::load_native_certs());
        t!("build no builtin roots", reqwest::Client::builder().tls_built_in_root_certs(false).build().unwrap());
        let certs: Vec<reqwest::Certificate> = rustls_native_certs::load_native_certs().certs.iter().filter_map(|c| reqwest::Certificate::from_der(c.as_ref()).ok()).collect();
        out.push_str(&format!("certs: {}\n", certs.len()));
        t!("build with cached certs", { let mut b = reqwest::Client::builder().tls_built_in_root_certs(false); for c in &certs { b = b.add_root_certificate(c.clone()); } b.build().unwrap() });
        t!("tempdir", tempfile::tempdir().unwrap());
        t!("Client::new", reqwest::Client::new());
        t!("Client::builder.build", reqwest::Client::builder().build().unwrap());
        t!("HubConfig::from_map", HubConfig::from_map(&vars).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let mut k = 0;
        t!("Store::open sqlite", {
            k += 1;
            let p = dir.path().join(format!("s{k}.db"));
            Store::open("sqlite", Some(p.to_str().unwrap())).unwrap()
        });
        t!("AppState::with_seams nostore", AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::Token("t".to_string()))),
        ).unwrap());
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::Token("t".to_string()))),
        ).unwrap();
        t!("build_router_with", build_router_with(state.clone(), None));
        t!("ExtensionSet builtins", crate::ext::ExtensionSet::new(crate::ext::builtin::builtins()));
        t!("ModelLimits::load", crate::services::model_limits::ModelLimits::load(None, std::iter::empty::<&str>(), 0));
        panic!("PROFILE\n{out}");
    }

    use std::sync::Arc;

    use serde_json::{Value, json};

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
    async fn local_engine_route_updates_the_registry_and_model_list_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("registry.txt");
        std::fs::write(&registry, "m|org/m|m.gguf|1|4096||||llama\n").unwrap();
        let models = dir.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("m.gguf"), vec![0_u8; 8]).unwrap();
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "local"),
            ("LLM_HUB_LOCAL_KIND", "local"),
            ("LLM_HUB_LOCAL_REGISTRY", registry.to_str().unwrap()),
            ("LLM_HUB_LOCAL_MODELS_DIR", models.to_str().unwrap()),
            ("LLM_HUB_LOCAL_ALLOW_UNGATED", "true"),
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = build_router(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let client = reqwest::Client::new();

        let put = client
            .put(format!("{base}/api/local/models/local/m/engine"))
            .json(&json!({"engine":"mistralrs"}))
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), reqwest::StatusCode::OK);
        assert_eq!(put.json::<Value>().await.unwrap()["engine"], "mistralrs");

        let list = client
            .get(format!("{base}/v1/models"))
            .send()
            .await
            .unwrap();
        let body = list.json::<Value>().await.unwrap();
        let local = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "local/m")
            .unwrap();
        assert_eq!(local["engine"], "mistralrs");
    }

    #[tokio::test]
    async fn http_rules_run_on_every_route_including_the_fallback_and_a_rejection() {
        let (base, state) = serve_hub_with_auth_state().await;
        let env = |_: &str| None;
        let rules = crate::ext::rules::parse(
            "rules:\n  - name: tag\n    phase: http_response\n    actions:\n      - set_header: {x-hub: on}\n  - name: block\n    phase: http_request\n    match: {path: /api/*, header: {x-block: '1'}}\n    actions:\n      - reject: {status: 403, message: blocked}\n  - name: accepted\n    phase: http_response\n    match: {path: /healthz, method: GET}\n    actions:\n      - set_status: 202\n      - remove_header: content-type\n  - name: trial\n    phase: http_request\n    mode: shadow\n    actions:\n      - reject: {status: 500, message: never}\n",
            false,
            &env,
        )
        .unwrap();
        let mut all = crate::ext::builtin::builtins();
        all.push(Arc::new(rules));
        state.set_extensions(crate::ext::ExtensionSet::new(all));
        let client = reqwest::Client::new();

        let health = client.get(format!("{base}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), 202);
        assert_eq!(health.headers()["x-hub"], "on");
        assert!(health.headers().get("content-type").is_none());
        assert_eq!(health.text().await.unwrap(), "ok");

        let page = client
            .get(format!("{base}/no-such-page"))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert_eq!(page.headers()["x-hub"], "on");

        let blocked = client
            .get(format!("{base}/api/profiles"))
            .bearer_auth(MASTER_KEY)
            .header("x-block", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), 403);
        assert_eq!(blocked.headers()["x-hub"], "on");
        let body: serde_json::Value = blocked.json().await.unwrap();
        assert_eq!(body["error"]["message"], "blocked");

        let blocked_before_the_key_check = client
            .get(format!("{base}/api/profiles"))
            .header("x-block", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(blocked_before_the_key_check.status(), 403);

        let allowed = client
            .get(format!("{base}/api/profiles"))
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(allowed.status(), 200);
        assert_eq!(allowed.headers()["x-hub"], "on");
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
    async fn openapi_and_docs_ui_follow_the_master_key_rule() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        for path in ["/openapi.json", "/api-docs/"] {
            let denied = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(denied.status(), 401, "{path} without a key");
            let ok = client
                .get(format!("{base}{path}"))
                .bearer_auth(MASTER_KEY)
                .send()
                .await
                .unwrap();
            assert_eq!(ok.status(), 200, "{path} with the key");
        }
        let spec: serde_json::Value = client
            .get(format!("{base}/openapi.json"))
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(spec["openapi"].as_str().unwrap().starts_with("3.1"));
        let health = client.get(format!("{base}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), 200);
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
    async fn sessions_routes_require_master_key() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        for url in [
            format!("{base}/api/sessions?range=7d"),
            format!("{base}/api/sessions/some-worker"),
        ] {
            let with_key = client
                .get(&url)
                .bearer_auth(MASTER_KEY)
                .send()
                .await
                .unwrap();
            let without_key = client.get(&url).send().await.unwrap();

            assert_eq!(with_key.status().as_u16(), 400, "{url}");
            assert_eq!(without_key.status().as_u16(), 401, "{url}");
        }
    }

    #[tokio::test]
    async fn live_route_requires_master_key() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/live");

        let with_key = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 200);
        assert!(
            with_key.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        assert_eq!(without_key.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn live_route_streams_each_finished_request() {
        let (base, state) = serve_hub_with_auth_state().await;
        let client = reqwest::Client::new();
        let mut response = client
            .get(format!("{base}/api/live"))
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        state.publish_live(&crate::services::stats::RequestOutcome {
            profile: "copilot".into(),
            model_key: "copilot/claude-opus-4-5".into(),
            status: 200,
            ts_ms: 0,
            latency_ms: 5,
            ttft_ms: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: Some("claude-code".into()),
            session_id: None,
            worker: None,
            routed_by: None,
            attempt: None,
            role: None,
        });

        let chunk = response.chunk().await.unwrap().unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        let payload = text
            .strip_prefix("data: ")
            .unwrap()
            .strip_suffix("\n\n")
            .unwrap();
        let row: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(row["model"], "copilot/claude-opus-4-5");
        assert_eq!(row["status"], 200);
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

    #[tokio::test]
    async fn copilot_usage_history_route_is_protected() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/copilot/usage/history?range=7d");

        let with_key = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&url).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 400);
        assert_eq!(without_key.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn keep_warm_route_is_protected() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/keep-warm");

        let with_key = client
            .post(&url)
            .bearer_auth(MASTER_KEY)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let without_key = client.post(&url).json(&json!({})).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 422);
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
            effort: None,
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

    #[tokio::test]
    async fn smart_auto_routes_are_protected() {
        use crate::schemas::scope_kind::ScopeKind;
        use crate::schemas::selection_event::SelectionEvent;
        use crate::schemas::selection_stage::SelectionStage;
        use crate::schemas::selection_state::SelectionState;

        let (base, state) = serve_hub_with_auth_state().await;
        let log = state.selection_log();
        let worker = log.scope_key(ScopeKind::Worker, "session-1");
        log.record(SelectionEvent {
            worker: Some(worker),
            ..SelectionEvent::new(
                SelectionStage::Solver,
                SelectionState::Selected,
                "copilot/claude-opus-4-5",
            )
        });
        let client = reqwest::Client::new();
        let current = format!("{base}/api/smart-auto/current?session=session-1");

        let with_key = client
            .get(&current)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();
        let without_key = client.get(&current).send().await.unwrap();

        assert_eq!(with_key.status().as_u16(), 200);
        let body: serde_json::Value = with_key.json().await.unwrap();
        assert_eq!(body["selection"]["state"], "selected");
        assert_eq!(
            body["selection"]["main"]["model"],
            "copilot/claude-opus-4-5"
        );
        assert_eq!(body["selection"]["stale"], false);
        assert_eq!(without_key.status().as_u16(), 401);

        for route in [
            "/api/smart-auto/history",
            "/api/smart-auto/decisions",
            "/api/smart-auto/health",
            "/api/smart-auto/tiers",
        ] {
            let url = format!("{base}{route}");
            let with_key = client
                .get(&url)
                .bearer_auth(MASTER_KEY)
                .send()
                .await
                .unwrap();
            let without_key = client.get(&url).send().await.unwrap();
            assert_eq!(with_key.status().as_u16(), 200, "{route}");
            assert_eq!(without_key.status().as_u16(), 401, "{route}");
        }
    }

    #[tokio::test]
    async fn smart_auto_resolve_requires_the_master_key() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_hub_with_rules(dir.path(), None).await;
        let url = format!("{base}/api/smart-auto/resolve");
        let client = reqwest::Client::new();
        let body = serde_json::json!({"tier": "fast"});

        let without_key = client.post(&url).json(&body).send().await.unwrap();
        let with_key = client
            .post(&url)
            .bearer_auth(MASTER_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(without_key.status().as_u16(), 401);
        assert_ne!(with_key.status().as_u16(), 401);
    }

    async fn serve_hub_with_rules(store_dir: &std::path::Path, env_rules: Option<&str>) -> String {
        let mut vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_MASTER_KEY", MASTER_KEY),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        vars.insert(
            "LLM_HUB_STORE_PATH".into(),
            store_dir.join("hub.db").to_string_lossy().into_owned(),
        );
        if let Some(rules) = env_rules {
            vars.insert("LLM_HUB_SMART_AUTO_RULES".into(), rules.into());
        }
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = build_router(state);
        tokio::spawn(async move { axum::serve(listener, router).await });
        base
    }

    #[tokio::test]
    async fn client_outcome_lands_on_the_workers_last_completed_attempt_in_history() {
        use crate::schemas::scope_kind::ScopeKind;
        use crate::schemas::selection_event::SelectionEvent;
        use crate::schemas::selection_stage::SelectionStage;
        use crate::schemas::selection_state::SelectionState;

        let dir = tempfile::tempdir().unwrap();
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_MASTER_KEY", MASTER_KEY),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .chain([
            (
                "LLM_HUB_STORE_PATH".to_string(),
                dir.path().join("hub.db").to_string_lossy().into_owned(),
            ),
            ("LLM_HUB_PERSISTENT".to_string(), "1".to_string()),
        ])
        .collect();
        let store = crate::services::store::Store::open(
            "sqlite",
            Some(dir.path().join("hub.db").to_str().unwrap()),
        )
        .unwrap();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            Some(store),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = build_router(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });

        let log = state.selection_log();
        let worker = log.scope_key(ScopeKind::Worker, "session-9");
        log.record(SelectionEvent {
            worker: Some(worker.clone()),
            attempt: Some("attempt-1".into()),
            ..SelectionEvent::new(
                SelectionStage::Solver,
                SelectionState::Completed,
                "copilot/claude-opus-4-5",
            )
        });
        state.attempt_tracker().note_completed(&worker, "attempt-1");
        let client = reqwest::Client::new();
        let outcome_url = format!("{base}/api/smart-auto/outcome");

        let missing = client
            .post(&outcome_url)
            .bearer_auth(MASTER_KEY)
            .json(&serde_json::json!({"session_id": "other", "code": "user_accepted"}))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status().as_u16(), 404);
        let bad_code = client
            .post(&outcome_url)
            .bearer_auth(MASTER_KEY)
            .json(&serde_json::json!({"session_id": "session-9", "code": "great"}))
            .send()
            .await
            .unwrap();
        assert!(bad_code.status().is_client_error());
        let saved = client
            .post(&outcome_url)
            .bearer_auth(MASTER_KEY)
            .json(&serde_json::json!({"session_id": "session-9", "code": "user_corrected"}))
            .send()
            .await
            .unwrap();
        assert_eq!(saved.status().as_u16(), 200);
        let unauthenticated = client
            .post(&outcome_url)
            .json(&serde_json::json!({"attempt": "attempt-1", "code": "user_accepted"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status().as_u16(), 401);

        let history: serde_json::Value = client
            .get(format!("{base}/api/smart-auto/history"))
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let event = &history["events"][0];
        assert_eq!(event["attempt"], "attempt-1");
        assert_eq!(event["outcome"]["code"], "user_corrected");
        assert_eq!(event["outcome"]["source"], "client");
    }

    #[tokio::test]
    async fn smart_auto_rules_save_to_the_file_and_env_locks_them() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_hub_with_rules(dir.path(), None).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/api/smart-auto/rules");

        let empty: serde_json::Value = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(empty["source"], "none");
        assert_eq!(empty["readonly"], false);

        for body in [r#"{"exclude":[""]}"#, r#"{"include":["a b"]}"#, "{"] {
            let rejected = client
                .put(&url)
                .bearer_auth(MASTER_KEY)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(rejected.status(), 400, "{body}");
        }
        let saved = client
            .put(&url)
            .bearer_auth(MASTER_KEY)
            .json(&json!({
                "include": ["copilot/*"],
                "profiles": {"copilot": {"exclude": ["gpt-*"]}}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(saved.status(), 200);
        assert!(dir.path().join("smart-auto-rules.json").exists());
        let reread: serde_json::Value = client
            .get(&url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reread["source"], "file");
        assert_eq!(reread["rules"]["include"], json!(["copilot/*"]));
        assert_eq!(
            reread["rules"]["profiles"]["copilot"]["exclude"],
            json!(["gpt-*"])
        );

        let locked_base =
            serve_hub_with_rules(dir.path(), Some(r#"{"include":["llama_swap/*"]}"#)).await;
        let locked_url = format!("{locked_base}/api/smart-auto/rules");
        let locked: serde_json::Value = client
            .get(&locked_url)
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(locked["source"], "env");
        assert_eq!(locked["readonly"], true);
        assert_eq!(locked["rules"]["include"], json!(["llama_swap/*"]));
        let conflict = client
            .put(&locked_url)
            .bearer_auth(MASTER_KEY)
            .json(&json!({"include": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict.status(), 409);
    }

    #[tokio::test]
    async fn smart_auto_current_needs_a_worker_or_session() {
        let base = serve_hub_with_auth().await;
        let client = reqwest::Client::new();

        let response = client
            .get(format!("{base}/api/smart-auto/current"))
            .bearer_auth(MASTER_KEY)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status().as_u16(), 400);
    }
}

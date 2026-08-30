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
use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{any, delete, get, post};

use crate::configs::HubConfig;
use crate::consts::{ENV_FILE, VERSION};
use crate::dependencies::state::AppState;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("llm_hub=info")),
        )
        .init();

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

    routes::models::spawn_background_refresh(state.clone());
    let restart_notify = Arc::new(tokio::sync::Notify::new());
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
                () = wait_for_ctrl_c() => {}
                () = restart_notify.notified() => {}
            }
        })
        .await
    {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

async fn wait_for_ctrl_c() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Handles `--version`, `update`, and `service` invocations.
/// Returns true when a CLI command was handled and the process should exit.
async fn run_cli_command() -> bool {
    match std::env::args().nth(1).as_deref() {
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
        _ => false,
    }
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
    let protected = Router::new()
        .route("/v1/models", get(routes::models::list_models))
        .route("/v1/models/{*id}", get(routes::proxy::get_model_by_id))
        .route("/v1/{*path}", any(routes::proxy::proxy))
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
        .route("/api/stats", get(routes::admin::stats))
        .route("/api/usage", get(routes::admin::usage))
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

//! Loads one GGUF model with mistral.rs and serves it on loopback.

use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use mistralrs_core::{AutoDeviceMapParams, ModelSelected};
use mistralrs_server_core::mistralrs_for_server_builder::MistralRsForServerBuilder;
use mistralrs_server_core::mistralrs_server_router_builder::MistralRsServerRouterBuilder;
use serde_json::{Value, json};

use super::WorkerArgs;
use super::request_defaults::{SamplingDefaults, fill_request_body};

/// The chat route whose request body gets the sampling defaults.
const CHAT_PATH: &str = "/v1/chat/completions";
/// The largest chat request body the worker reads before it forwards the request.
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Serves `args.model` until the process gets SIGTERM or Ctrl-C.
///
/// # Errors
///
/// Returns a message when the model fails to load, the router cannot be built, or the port
/// cannot bind.
pub async fn serve(args: WorkerArgs) -> Result<(), String> {
    let model = model_selected(&args)?;
    let max_seq_len = usize::try_from(args.options.ctx_size)
        .map_err(|_| format!("model {}: --ctx-size does not fit in usize", args.id))?;
    let state = MistralRsForServerBuilder::new()
        .with_model(model)
        .with_model_id_override(args.id.clone())
        .with_max_model_len(max_seq_len)
        .build()
        .await
        .map_err(|err| format!("model {}: load failed: {err:#}", args.id))?;
    let router = MistralRsServerRouterBuilder::new()
        .with_mistralrs(state)
        .build()
        .await
        .map_err(|err| format!("model {}: router failed: {err:#}", args.id))?;
    let props_document = props_document(&args);
    let router = router.route(
        "/props",
        get(move || {
            let document = props_document.clone();
            async move { Json(document) }
        }),
    );
    let router = router.layer(middleware::from_fn_with_state(
        Arc::new(SamplingDefaults::from_options(&args.options)),
        fill_chat_defaults,
    ));
    let address = format!("127.0.0.1:{}", args.port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|err| format!("model {}: cannot bind {address}: {err}", args.id))?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| format!("model {}: server stopped: {err}", args.id))
}

/// The `ModelSelected::GGUF` value of `args`, built from its serde form because the crate
/// exposes no constructor for it.
fn model_selected(args: &WorkerArgs) -> Result<ModelSelected, String> {
    let directory = args.model.parent().unwrap_or(Path::new("."));
    let file = args
        .model
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("model {}: the model path has no file name", args.id))?;
    let directory = directory
        .to_str()
        .ok_or_else(|| format!("model {}: the model directory is not UTF-8", args.id))?;
    let value = json!({
        "GGUF": {
            "quantized_model_id": directory,
            "quantized_filename": file,
            "tok_model_id": args.options.tok_model_id,
            "mmproj_filename": args.options.mmproj.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "dtype": "auto",
            "max_seq_len": args.options.ctx_size,
            "max_batch_size": AutoDeviceMapParams::DEFAULT_MAX_BATCH_SIZE,
        }
    });
    serde_json::from_value(value).map_err(|err| format!("model {}: {err}", args.id))
}

/// The `/props` document: the context length, as llama-server reports it.
fn props_document(args: &WorkerArgs) -> Value {
    json!({
        "n_ctx": args.options.ctx_size,
        "total_slots": 1,
        "model_path": args.model.to_string_lossy(),
    })
}

/// Fills the sampling defaults and thinking switch into a chat request, then forwards it.
async fn fill_chat_defaults(
    State(defaults): State<Arc<SamplingDefaults>>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST || request.uri().path() != CHAT_PATH {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let bytes = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return axum::http::StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let filled = fill_request_body(&bytes, &defaults);
    parts.headers.remove(header::CONTENT_LENGTH);
    next.run(Request::from_parts(parts, Body::from(filled)))
        .await
}

/// Resolves on SIGTERM (the hub's stop signal) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

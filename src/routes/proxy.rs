use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::consts::{HEADER_ATTEMPTS, HEADER_SERVED_MODEL, USAGE_SCRAPE_TAIL_BYTES};
use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::model_id::ModelId;
use crate::services::fallback::{RetryPolicy, fallback_chain, per_attempt_timeout_ms};
use crate::services::stats::{RequestOutcome, scrape_usage};
use crate::utils::headers::{filter_request_headers, filter_response_headers};

/// Catch-all /v1/* passthrough with header-driven fallbacks.
pub async fn proxy(State(state): State<AppState>, req: Request) -> Result<Response, AppError> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    let content_type = header_value(&parts.headers, "content-type");
    if content_type.starts_with("multipart/") {
        return Err(AppError::NotImplemented(
            "multipart endpoints (audio/files) are not supported yet".to_string(),
        ));
    }

    let max_replay = state.config().max_replay_bytes;
    let (buffered, overflow_stream) = read_body_capped(body, max_replay).await?;

    match overflow_stream {
        // Over the replay cap: stream straight through, fallbacks disabled.
        Some(rest) => {
            passthrough_oversized(
                &state,
                parts.method,
                &path_and_query,
                &parts.headers,
                buffered,
                rest,
            )
            .await
        }
        None => {
            attempt_loop(
                &state,
                parts.method,
                &path_and_query,
                &parts.headers,
                buffered,
            )
            .await
        }
    }
}

/// GET /v1/models/{id...} — routes by the id's first segment.
pub async fn get_model_by_id(
    State(state): State<AppState>,
    req: Request,
) -> Result<Response, AppError> {
    let (parts, _) = req.into_parts();
    let path = parts.uri.path().trim_start_matches("/v1/models/");
    let id = ModelId::parse(path).ok_or_else(|| {
        AppError::BadRequest(format!("model id must be <profile>/<model>, got: {path}"))
    })?;
    let target = format!("/v1/models/{}", id.model);
    attempt_single(
        &state,
        Method::GET,
        &target,
        &parts.headers,
        Bytes::new(),
        &id,
        None,
    )
    .await
}

async fn attempt_loop(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let config = state.config();
    let mut json_body: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };

    let primary = json_body
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(primary) = primary else {
        // No model in the body (e.g. GET without body): need a profile from the path? Reject clearly.
        return Err(AppError::BadRequest(
            "request body must contain a \"model\" field shaped <profile>/<model>".to_string(),
        ));
    };

    let primary_id = parse_model_or_400(&config, &primary)?;
    inject_stream_usage(json_body.as_mut());

    let mut chain: Vec<ModelId> = vec![primary_id];
    for fallback in fallback_chain(headers, &config.default_fallbacks) {
        if let Some(id) = ModelId::parse(&fallback) {
            chain.push(id);
        }
    }

    let retry_policy = RetryPolicy::from_headers(headers);
    let attempt_timeout = per_attempt_timeout_ms(headers).map(Duration::from_millis);

    let mut attempts_trail: Vec<String> = Vec::new();
    let last_index = chain.len() - 1;

    for (index, model_id) in chain.iter().enumerate() {
        let attempt_body = body_for_attempt(json_body.as_ref(), &body, model_id);
        let started = Instant::now();
        let result = send_attempt(
            state,
            &method,
            path_and_query,
            headers,
            attempt_body,
            model_id,
            attempt_timeout,
        )
        .await;

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                attempts_trail.push(format!("{}={status}", model_id.qualified()));
                let final_attempt =
                    index == last_index || !retry_policy.should_retry_status(status);
                if final_attempt {
                    return Ok(build_response(
                        state,
                        response,
                        model_id,
                        &attempts_trail,
                        started,
                    ));
                }
                record_failure(state, model_id, status, started);
            }
            Err(reason) => {
                attempts_trail.push(format!("{}={reason}", model_id.qualified()));
                record_failure(state, model_id, 0, started);
                if index == last_index {
                    return Err(AppError::UpstreamUnavailable(format!(
                        "all attempts failed: {}",
                        attempts_trail.join(", ")
                    )));
                }
            }
        }
    }
    unreachable!("attempt loop always returns on the last attempt");
}

async fn attempt_single(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
    model_id: &ModelId,
    attempt_timeout: Option<Duration>,
) -> Result<Response, AppError> {
    let started = Instant::now();
    let trail = vec![model_id.qualified()];
    let response = send_attempt(
        state,
        &method,
        path_and_query,
        headers,
        body,
        model_id,
        attempt_timeout,
    )
    .await
    .map_err(AppError::UpstreamUnavailable)?;
    Ok(build_response(state, response, model_id, &trail, started))
}

fn parse_model_or_400(config: &crate::configs::HubConfig, raw: &str) -> Result<ModelId, AppError> {
    let Some(id) = ModelId::parse(raw) else {
        let profiles: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
        return Err(AppError::BadRequest(format!(
            "model must be <profile>/<model>; got \"{raw}\". valid profiles: {}",
            profiles.join(", ")
        )));
    };
    Ok(id)
}

/// Q3 decision: ask upstreams for the usage chunk on streams so token stats
/// work, unless the caller already set stream_options themselves.
fn inject_stream_usage(json_body: Option<&mut Value>) {
    let Some(Value::Object(obj)) = json_body else {
        return;
    };
    let is_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !is_stream || obj.contains_key("stream_options") {
        return;
    }
    obj.insert(
        "stream_options".into(),
        serde_json::json!({ "include_usage": true }),
    );
}

fn body_for_attempt(json_body: Option<&Value>, original: &Bytes, model_id: &ModelId) -> Bytes {
    let Some(json) = json_body else {
        return original.clone();
    };
    let mut attempt = json.clone();
    if let Some(obj) = attempt.as_object_mut() {
        obj.insert("model".into(), Value::String(model_id.model.clone()));
    }
    Bytes::from(serde_json::to_vec(&attempt).unwrap_or_else(|_| original.to_vec()))
}

async fn send_attempt(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
    model_id: &ModelId,
    attempt_timeout: Option<Duration>,
) -> Result<reqwest::Response, String> {
    let config = state.config();
    let profile = config
        .profile(&model_id.profile)
        .ok_or_else(|| format!("unknown profile: {}", model_id.profile))?;
    if !profile.enabled {
        return Err(format!("profile disabled: {}", profile.name));
    }
    let client = state.client(&profile.name).ok_or("no client for profile")?;

    let upstream_path = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
    let url = format!("{}{}", profile.base_url, upstream_path);

    let mut request = client
        .request(method.clone(), url)
        .headers(to_reqwest_headers(&filter_request_headers(headers)));
    if !profile.api_key.is_empty() {
        request = request.bearer_auth(&profile.api_key);
    }
    for (name, value) in &profile.extra_headers {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }

    let effective_timeout = attempt_timeout.or(profile.timeout_ms.map(Duration::from_millis));
    let send = request.send();
    let response = match effective_timeout {
        Some(timeout) => tokio::time::timeout(timeout, send)
            .await
            .map_err(|_| "timeout".to_string())?,
        None => send.await,
    };
    response.map_err(|e| format!("connect error: {e}"))
}

/// Streams the upstream response through unbuffered, observing a rolling
/// tail for usage scraping and recording stats when the stream ends.
fn build_response(
    state: &AppState,
    upstream: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
) -> Response {
    let status = upstream.status();
    let mut response_headers = filter_response_headers(&headermap_from_reqwest(upstream.headers()));
    insert_header(
        &mut response_headers,
        HEADER_SERVED_MODEL,
        &model_id.qualified(),
    );
    insert_header(
        &mut response_headers,
        HEADER_ATTEMPTS,
        &attempts_trail.join(", "),
    );

    let state_for_stats = state.clone();
    let outcome_seed = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key: model_id.qualified(),
        status: status.as_u16(),
        latency_ms: 0,
        tokens_in: 0,
        tokens_out: 0,
    };

    let (tail_sender, tail_receiver) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let observed = futures_util::stream::unfold(
        (upstream.bytes_stream(), Vec::new(), Some(tail_sender)),
        |(mut inner, mut tail, mut sender)| async move {
            match inner.next().await {
                Some(chunk) => {
                    if let Ok(bytes) = &chunk {
                        append_tail(&mut tail, bytes);
                    }
                    Some((chunk, (inner, tail, sender)))
                }
                None => {
                    if let Some(s) = sender.take() {
                        let _ = s.send(tail);
                    }
                    None
                }
            }
        },
    );

    tokio::spawn(async move {
        let tail_bytes = tail_receiver.await.unwrap_or_default();
        let (tokens_in, tokens_out) = scrape_usage(&tail_bytes);
        let outcome = RequestOutcome {
            latency_ms: crate::utils::time::elapsed_ms(started),
            tokens_in,
            tokens_out,
            ..outcome_seed
        };
        state_for_stats.stats().record(&outcome);
        if let Some(store) = state_for_stats.store()
            && let Err(e) = store.record(&outcome)
        {
            tracing::warn!("store record failed: {e}");
        }
    });

    let mut builder = Response::builder().status(status);
    if let Some(headers_mut) = builder.headers_mut() {
        *headers_mut = response_headers;
    }
    builder
        .body(Body::from_stream(observed))
        .unwrap_or_else(|_| {
            let mut fallback = Response::new(Body::empty());
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

fn record_failure(state: &AppState, model_id: &ModelId, status: u16, started: Instant) {
    let outcome = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key: model_id.qualified(),
        status: if status == 0 { 599 } else { status },
        latency_ms: crate::utils::time::elapsed_ms(started),
        tokens_in: 0,
        tokens_out: 0,
    };
    state.stats().record(&outcome);
    if let Some(store) = state.store() {
        let _ = store.record(&outcome);
    }
}

async fn passthrough_oversized(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    buffered: Bytes,
    rest: axum::body::BodyDataStream,
) -> Result<Response, AppError> {
    // Model extraction needs the JSON prefix; for oversized bodies we try the
    // buffered prefix, which contains the model field in practice.
    let model = extract_model_prefix(&buffered).ok_or_else(|| {
        AppError::BadRequest("could not find \"model\" in request body prefix".to_string())
    })?;
    let config = state.config();
    let model_id = parse_model_or_400(&config, &model)?;

    // NOTE: the model field is NOT rewritten in oversized mode (body is not
    // re-serialized); upstreams that reject unknown model ids will 404.
    // Documented limitation — replay cap exists to bound memory.
    let profile = config
        .profile(&model_id.profile)
        .ok_or_else(|| AppError::BadRequest(format!("unknown profile: {}", model_id.profile)))?;
    let client = state
        .client(&profile.name)
        .ok_or_else(|| AppError::Internal("no client for profile".to_string()))?;

    let upstream_path = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
    let url = format!("{}{}", profile.base_url, upstream_path);

    let combined = futures_util::stream::iter(vec![Ok::<Bytes, axum::Error>(buffered)]).chain(rest);
    let mut request = client
        .request(method, url)
        .headers(to_reqwest_headers(&filter_request_headers(headers)))
        .body(reqwest::Body::wrap_stream(combined));
    if !profile.api_key.is_empty() {
        request = request.bearer_auth(&profile.api_key);
    }
    for (name, value) in &profile.extra_headers {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| AppError::UpstreamUnavailable(e.to_string()))?;
    let started = Instant::now();
    Ok(build_response(
        state,
        response,
        &model_id,
        &[model_id.qualified()],
        started,
    ))
}

/// Reads the body up to `cap` bytes. Returns (buffered, Some(rest)) when the
/// body exceeds the cap — the caller must stream `buffered + rest` through.
async fn read_body_capped(
    body: Body,
    cap: usize,
) -> Result<(Bytes, Option<axum::body::BodyDataStream>), AppError> {
    let mut stream = body.into_data_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AppError::BadRequest(format!("body read error: {e}")))?;
        buffered.extend_from_slice(&chunk);
        if buffered.len() > cap {
            return Ok((Bytes::from(buffered), Some(stream)));
        }
    }
    Ok((Bytes::from(buffered), None))
}

fn extract_model_prefix(buffered: &Bytes) -> Option<String> {
    let text = String::from_utf8_lossy(buffered);
    let pos = text.find("\"model\"")?;
    let after = &text[pos + "\"model\"".len()..];
    let start = after.find('"')? + 1;
    let end = after[start..].find('"')? + start;
    Some(after[start..end].to_string())
}

fn append_tail(tail: &mut Vec<u8>, bytes: &Bytes) {
    tail.extend_from_slice(bytes);
    let overflow = tail.len().saturating_sub(USAGE_SCRAPE_TAIL_BYTES);
    if overflow > 0 {
        tail.drain(..overflow);
    }
}

// --- small adapters ---

fn header_value(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_lowercase()
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(header_value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), header_value);
    }
}

fn to_reqwest_headers(headers: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out
}

fn headermap_from_reqwest(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(n, v);
        }
    }
    out
}

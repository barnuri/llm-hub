use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::consts::{
    ANTHROPIC_UPSTREAM_PATH, HEADER_ATTEMPTS, HEADER_SERVED_MODEL, HEADER_TRANSFORMS,
    MAX_TRANSFORM_BUFFER_BYTES, USAGE_SCRAPE_TAIL_BYTES,
};
use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::model_id::ModelId;
use crate::services::anthropic;
use crate::services::body_transform::BodyTransform;
use crate::services::fallback::{RetryPolicy, fallback_chain, per_attempt_timeout_ms};
use crate::services::stats::{RequestOutcome, ScrapedUsage, scrape_usage};
use crate::services::transforms::{self, RouteKind, TransformPlan};
use crate::utils::headers::{filter_request_headers, filter_response_headers};

/// Catch-all /v1/* passthrough with header-driven fallbacks.
pub async fn proxy(State(state): State<AppState>, req: Request) -> Result<Response, AppError> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts.uri.path_and_query().map_or_else(
        || parts.uri.path().to_string(),
        |pq| pq.as_str().to_string(),
    );

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
                RouteKind::OpenAi,
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

/// The single fallback loop, shared by every client-facing API shape.
/// `route` decides two things and nothing else: whether the body is translated
/// from Anthropic Messages on the way in, and whether the winning response is
/// translated back on the way out.
///
/// # Errors
/// `AppError::BadRequest` when the body carries no usable `model` (or, on the
/// Anthropic route, fails translation); `AppError::UpstreamUnavailable` when
/// every attempt in the chain fails to produce a response.
pub(crate) async fn attempt_loop(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
    route: RouteKind,
) -> Result<Response, AppError> {
    let config = state.config();
    let mut json_body: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice(&body).ok()
    };

    // Translate first so everything downstream sees exactly one body shape.
    if route == RouteKind::Anthropic {
        json_body = Some(anthropic::request::to_openai(json_body.as_ref())?);
    }

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
    inject_stream_usage(&mut json_body);

    // The single request-side transform seam. It runs once, before the chain is
    // built, so every attempt sends byte-identical bytes.
    let plan = transforms::apply_request(&mut json_body, headers, config.stream_role_inject);
    // Whenever the hub reads the response body itself it must be handed
    // uncompressed bytes; passthrough requests keep the caller's header.
    let force_identity_encoding = route == RouteKind::Anthropic || plan.wants_response_transform();

    // The Anthropic route always targets the upstream's chat-completions
    // endpoint, whatever the client-facing path was.
    let upstream_path = match route {
        RouteKind::OpenAi => path_and_query,
        RouteKind::Anthropic => ANTHROPIC_UPSTREAM_PATH,
    };

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
            upstream_path,
            headers,
            attempt_body,
            model_id,
            attempt_timeout,
            force_identity_encoding,
        )
        .await;

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                attempts_trail.push(format!("{}={status}", model_id.qualified()));
                let final_attempt =
                    index == last_index || !retry_policy.should_retry_status(status);
                if final_attempt {
                    return Ok(deliver(
                        state,
                        response,
                        model_id,
                        &attempts_trail,
                        started,
                        route,
                        &plan,
                    )
                    .await);
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

/// Renders the winning upstream response for the client-facing API shape.
///
/// The SSE decision is the upstream's content-type, not the request's `stream`
/// flag: an upstream that answers a streaming request with a JSON error must be
/// translated as a message, not fed to the SSE machinery.
async fn deliver(
    state: &AppState,
    response: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    route: RouteKind,
    plan: &TransformPlan,
) -> Response {
    let is_sse = is_event_stream(&response);
    // The Anthropic non-stream translation is the one body the hub buffers in
    // full; everything else streams through a `BodyTransform` (or, in the
    // common case, through nothing at all).
    let mut out = if route == RouteKind::Anthropic && !is_sse {
        build_anthropic_response(state, response, model_id, attempts_trail, started, plan).await
    } else {
        let transform = transforms::response_transform(route, plan, &model_id.qualified(), is_sse);
        build_response(
            state,
            response,
            model_id,
            attempts_trail,
            started,
            transform,
        )
    };
    if let Some(applied) = plan.header_value() {
        insert_header(out.headers_mut(), HEADER_TRANSFORMS, &applied);
    }
    out
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
        false,
    )
    .await
    .map_err(AppError::UpstreamUnavailable)?;
    Ok(build_response(
        state, response, model_id, &trail, started, None,
    ))
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
/// work, unless the caller already set `stream_options` themselves.
fn inject_stream_usage(json_body: &mut Option<Value>) {
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

/// `force_identity_encoding` is set whenever the hub itself has to read the
/// response body (the Anthropic route, and any request whose plan carries a
/// response-side transform). `reqwest` is built without the `gzip`/`brotli`
/// features and the caller's `accept-encoding` is forwarded as-is, so without
/// this the hub would be handed compressed bytes it cannot decode. The header
/// is *replaced* rather than added — appending would leave the caller's `gzip`
/// in place next to it and the upstream would still be free to compress.
/// Passthrough requests keep the caller's header untouched.
// One argument over the lint's threshold; a parameter struct for a single
// private call site would obscure more than it clarifies.
#[allow(clippy::too_many_arguments)]
async fn send_attempt(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
    model_id: &ModelId,
    attempt_timeout: Option<Duration>,
    force_identity_encoding: bool,
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

    let mut forwarded = filter_request_headers(headers);
    if force_identity_encoding {
        forwarded.insert("accept-encoding", HeaderValue::from_static("identity"));
    }
    let mut request = client
        .request(method.clone(), url)
        .headers(to_reqwest_headers(&forwarded));
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

/// Streams the upstream response through, observing a rolling tail for usage
/// scraping and recording stats when the stream ends.
///
/// `transform` is the only thing that decides whether the bytes are touched.
/// `None` — every route but the Anthropic one — is byte-for-byte passthrough
/// with no copy and no framing. `Some` rewrites frame by frame, and its
/// `finish()` output is emitted as a final chunk so the client is always left
/// with a well-formed body, even when the upstream is cut mid-stream.
fn build_response(
    state: &AppState,
    upstream: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    transform: Option<BodyTransform>,
) -> Response {
    let status = upstream.status();
    let mut response_headers = filter_response_headers(&headermap_from_reqwest(upstream.headers()));
    if transform.is_some() {
        // The body is rewritten, so the upstream's encoding no longer
        // describes it. `send_attempt` asked for `identity` in this case, but
        // strip the header regardless rather than trust the upstream to honour
        // it.
        response_headers.remove("content-encoding");
    }
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
        ttft_ms: None,
        tokens_in: 0,
        tokens_out: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };

    let (summary_sender, summary_receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
    let observed = futures_util::stream::unfold(
        StreamState {
            inner: upstream.bytes_stream(),
            tail: Vec::new(),
            transform,
            sender: Some(summary_sender),
            ended: false,
            started,
            ttft_ms: None,
        },
        pump,
    );

    tokio::spawn(async move {
        let summary = summary_receiver.await.unwrap_or_default();
        let scraped = scrape_usage(&summary.tail);
        let usage = match summary.observed_usage {
            Some(observed) => ScrapedUsage {
                tokens_in: observed.tokens_in,
                tokens_out: observed.tokens_out,
                cache_read_tokens: observed
                    .cache_read_tokens
                    .max(scraped.cache_read_tokens),
                cache_write_tokens: observed
                    .cache_write_tokens
                    .max(scraped.cache_write_tokens),
            },
            None => scraped,
        };
        let outcome = RequestOutcome {
            latency_ms: crate::utils::time::elapsed_ms(started),
            ttft_ms: summary.ttft_ms,
            tokens_in: usage.tokens_in,
            tokens_out: usage.tokens_out,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
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

/// One step of the response body stream: forward or rewrite the next upstream
/// chunk, and on end-of-upstream emit the transform's tail and hand the stats
/// task its summary.
async fn pump<S>(
    mut state: StreamState<S>,
) -> Option<(Result<Bytes, reqwest::Error>, StreamState<S>)>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    loop {
        if !state.ended {
            match state.inner.next().await {
                Some(Ok(bytes)) => {
                    // The tail always holds the *upstream* bytes, so usage
                    // scraping keeps working when a transform is active but
                    // observed nothing itself.
                    if state.ttft_ms.is_none() {
                        state.ttft_ms = Some(crate::utils::time::elapsed_ms(state.started));
                    }
                    append_tail(&mut state.tail, &bytes);
                    let Some(active) = state.transform.as_mut() else {
                        return Some((Ok(bytes), state));
                    };
                    let out = active.push(&bytes);
                    // An empty rewrite is normal (a partial frame) and must not
                    // be yielded: a zero-length chunk is not worth a frame.
                    if out.is_empty() {
                        continue;
                    }
                    return Some((Ok(Bytes::from(out)), state));
                }
                // A mid-stream read error is unrecoverable either way; with a
                // transform active the client is still owed a well-formed tail,
                // so the error ends the stream instead of being forwarded.
                Some(Err(e)) if state.transform.is_none() => return Some((Err(e), state)),
                Some(Err(e)) => {
                    tracing::warn!("upstream stream error, closing translated stream: {e}");
                    state.ended = true;
                }
                None => state.ended = true,
            }
        }

        let tail_chunk = state.transform.as_mut().map(BodyTransform::finish);
        if let Some(sender) = state.sender.take() {
            let _ = sender.send(StreamSummary {
                tail: std::mem::take(&mut state.tail),
                observed_usage: state
                    .transform
                    .as_ref()
                    .and_then(BodyTransform::observed_usage),
                ttft_ms: state.ttft_ms,
            });
        }
        state.transform = None;
        return match tail_chunk {
            Some(bytes) if !bytes.is_empty() => Some((Ok(Bytes::from(bytes)), state)),
            _ => None,
        };
    }
}

/// The per-response state the body stream carries while it drains.
struct StreamState<S> {
    inner: S,
    tail: Vec<u8>,
    transform: Option<BodyTransform>,
    sender: Option<tokio::sync::oneshot::Sender<StreamSummary>>,
    ended: bool,
    started: Instant,
    ttft_ms: Option<u64>,
}

/// What the finished body stream hands the stats task. `observed_usage` wins
/// when a transform reported real numbers — the translated Anthropic body uses
/// `input_tokens`/`output_tokens`, which older scrapers could not see.
#[derive(Default)]
struct StreamSummary {
    tail: Vec<u8>,
    observed_usage: Option<ScrapedUsage>,
    ttft_ms: Option<u64>,
}

/// True when the upstream answered with an SSE body.
fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("text/event-stream"))
}

/// Buffers the upstream chat-completions response and re-renders it in
/// Anthropic Messages shape. Buffering is safe here because this path is
/// non-SSE by construction — `deliver` sends event-stream responses to the
/// frame-at-a-time translator instead — and it is bounded by
/// `MAX_TRANSFORM_BUFFER_BYTES`. Every other route keeps the byte-for-byte
/// streaming path in `build_response`.
async fn build_anthropic_response(
    state: &AppState,
    upstream: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    plan: &TransformPlan,
) -> Response {
    let upstream_status = upstream.status();
    let mut response_headers = filter_response_headers(&headermap_from_reqwest(upstream.headers()));
    // The body is rewritten, so any upstream framing/encoding no longer describes it.
    response_headers.remove("content-encoding");
    insert_header(&mut response_headers, "content-type", "application/json");
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

    let (raw, overflowed) = read_upstream_capped(upstream, MAX_TRANSFORM_BUFFER_BYTES).await;
    let (status, payload) = if overflowed {
        tracing::warn!(
            "anthropic response exceeded {MAX_TRANSFORM_BUFFER_BYTES} bytes and cannot be translated"
        );
        (
            StatusCode::BAD_GATEWAY,
            anthropic::response::error_body(
                StatusCode::BAD_GATEWAY.as_u16(),
                "upstream response is too large to translate into the anthropic message shape",
            ),
        )
    } else {
        (
            upstream_status,
            // Truncated tool names go back to their originals as the last
            // stage, the same blunt quoted replace every other transform ends
            // with — here it catches the `tool_use.name` the translator just
            // emitted, and any echo of the name elsewhere in the payload.
            plan.names.restore_bytes(anthropic::response::to_anthropic(
                upstream_status.as_u16(),
                &raw,
                &model_id.qualified(),
            )),
        )
    };

    // Usage is scraped from the OpenAI-shaped body, before translation — the
    // Anthropic `input_tokens`/`output_tokens` names are also handled now, but
    // the upstream shape remains the primary source.
    let usage = scrape_usage(&raw);
    let latency_ms = crate::utils::time::elapsed_ms(started);
    let outcome = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key: model_id.qualified(),
        status: status.as_u16(),
        latency_ms,
        // Buffered replies have no first-token event; treat full latency as TTFT.
        ttft_ms: Some(latency_ms),
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
    };
    let state_for_stats = state.clone();
    tokio::spawn(async move {
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
    builder.body(Body::from(payload)).unwrap_or_else(|_| {
        let mut fallback = Response::new(Body::empty());
        *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        fallback
    })
}

/// Reads an upstream body into memory, stopping once `cap` is exceeded.
/// Returns `(bytes, overflowed)`; a mid-stream read error ends the read and is
/// reported as whatever arrived, which the translator then renders as an error.
async fn read_upstream_capped(upstream: reqwest::Response, cap: usize) -> (Vec<u8>, bool) {
    let mut stream = upstream.bytes_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buffered.extend_from_slice(&bytes);
        if buffered.len() > cap {
            return (buffered, true);
        }
    }
    (buffered, false)
}

fn record_failure(state: &AppState, model_id: &ModelId, status: u16, started: Instant) {
    let latency_ms = crate::utils::time::elapsed_ms(started);
    let outcome = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key: model_id.qualified(),
        status: if status == 0 { 599 } else { status },
        latency_ms,
        ttft_ms: None,
        tokens_in: 0,
        tokens_out: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
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
        None,
    ))
}

/// Reads the body up to `cap` bytes. Returns (buffered, Some(rest)) when the
/// body exceeds the cap — the caller must stream `buffered + rest` through.
pub(crate) async fn read_body_capped(
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

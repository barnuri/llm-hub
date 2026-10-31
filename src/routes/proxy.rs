use std::borrow::Cow;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::configs::ProfileConfig;
use crate::consts::{
    ANTHROPIC_UPSTREAM_PATH, AUTO_REASON_HEADER_MAX_CHARS, ERROR_REASON_MAX_CHARS,
    ERROR_STATUS_MIN, HEADER_ATTEMPTS, HEADER_AUTO_PICK, HEADER_AUTO_REASON, HEADER_SERVED_MODEL,
    HEADER_TRANSFORMS, MAX_MULTIPART_BYTES, MAX_TRANSFORM_BUFFER_BYTES, SSE_KEEPALIVE_FRAME,
    USAGE_SCRAPE_TAIL_BYTES,
};
use crate::dependencies::state::AppState;
use crate::routes::models::{COPILOT_AUTO_MODEL, advertised_bare_id};
use crate::schemas::app_error::AppError;
use crate::schemas::model_id::ModelId;
use crate::schemas::upstream_model::UpstreamModel;
use crate::services::anthropic;
use crate::services::anthropic::native_reply::NativeReplyRepair;
use crate::services::anthropic::responses_stream::ResponsesStream;
use crate::services::body_transform::BodyTransform;
use crate::services::copilot_auto_decisions::AutoDecision;
use crate::services::copilot_body_fields::BodyField;
use crate::services::fallback::{RetryPolicy, fallback_chain, per_attempt_timeout_ms};
use crate::services::harness::client_signature;
use crate::services::multipart_model;
use crate::services::responses_item_ids::ResponsesItemIds;
use crate::services::stats::{RequestOutcome, ScrapedUsage, scrape_usage};
use crate::services::store::now_ms;
use crate::services::tool_names::NameMap;
use crate::services::transforms::{self, RouteKind, TransformPlan};
use crate::services::upstream_auth;
use crate::utils::headers::{filter_request_headers, filter_response_headers};

/// Copilot serves Claude models on the Anthropic Messages API at this path.
const NATIVE_MESSAGES_PATH: &str = "/v1/messages";
const NATIVE_COUNT_TOKENS_PATH: &str = "/v1/messages/count_tokens";
const HEADER_ANTHROPIC_VERSION: &str = "anthropic-version";
/// Forwarded on native `/v1/messages` attempts only.
const HEADER_ANTHROPIC_BETA: &str = "anthropic-beta";
/// A Copilot "Extra inputs are not permitted" rejection is a short JSON error.
const MAX_REJECTION_BYTES: u64 = 64 * 1024;
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const HEADER_COPILOT_SESSION_TOKEN: &str = "copilot-session-token";
/// Claude Code names its conversation in this request header.
const HEADER_CLAUDE_CODE_SESSION_ID: &str = "x-claude-code-session-id";
const CLAUDE_CODE_SESSION_ID_MAX_LEN: usize = 128;
const IDENTITY_ENCODING: &str = "identity";
/// Copilot's Claude model ids, as sent upstream.
const CLAUDE_MODEL_PREFIX: &str = "claude";
/// Copilot's Responses API path, as sent upstream.
const UPSTREAM_RESPONSES_PATH: &str = "/responses";
/// Copilot's chat-completions endpoint, as `supported_endpoints` lists it.
const CHAT_COMPLETIONS_ENDPOINT: &str = "/chat/completions";
static NO_BODY: Value = Value::Null;

/// Catch-all /v1/* passthrough with header-driven fallbacks.
pub async fn proxy(State(state): State<AppState>, req: Request) -> Result<Response, AppError> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts.uri.path_and_query().map_or_else(
        || parts.uri.path().to_string(),
        |pq| pq.as_str().to_string(),
    );

    let is_multipart = header_value(&parts.headers, "content-type").starts_with("multipart/");
    let max_replay = if is_multipart {
        state.config().max_replay_bytes.max(MAX_MULTIPART_BYTES)
    } else {
        state.config().max_replay_bytes
    };
    let (buffered, overflow_stream) = read_body_capped(body, max_replay).await?;

    match overflow_stream {
        // The model field can only be rewritten in a buffered body.
        Some(_) if is_multipart => Err(AppError::BadRequest(format!(
            "multipart body exceeds {max_replay} bytes"
        ))),
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
    let (bodies, primary_id) =
        AttemptBodies::prepare(&config, path_and_query, headers, &body, route)?;

    let mut chain: Vec<ModelId> = vec![primary_id];
    for fallback in fallback_chain(headers, &config.default_fallbacks) {
        if let Some(id) = ModelId::parse(&fallback) {
            chain.push(id);
        }
    }

    let client = client_signature(headers);
    let retry_policy = RetryPolicy::from_headers(headers);
    let attempt_timeout = per_attempt_timeout_ms(headers).map(Duration::from_millis);

    let mut attempts_trail: Vec<String> = Vec::new();
    let last_index = chain.len() - 1;

    for (index, requested) in chain.iter().enumerate() {
        let auto = resolve_auto(state, &bodies, requested).await;
        let model_id = auto.as_ref().map_or(requested, |pick| &pick.model_id);
        let attempt = bodies.for_model(state, model_id);
        let started = Instant::now();
        let sender = AttemptSender {
            state,
            method: &method,
            attempt: &attempt,
            model_id,
            attempt_timeout,
            session_token: auto.as_ref().and_then(|pick| pick.session_token.as_deref()),
        };
        let result = match sender.send(attempt.body.clone()).await {
            Ok(response) if attempt.native_messages => {
                retry_without_rejected_fields(&bodies, &sender, response).await
            }
            Ok(response) => Ok(attempt.reply_rewrite.apply(response).await),
            other => other,
        };

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                attempts_trail.push(format!("{}={status}", model_id.qualified()));
                let final_attempt =
                    index == last_index || !retry_policy.should_retry_status(status);
                if final_attempt {
                    let mut out = deliver(
                        state,
                        response,
                        model_id,
                        &attempts_trail,
                        started,
                        &attempt,
                        client,
                    )
                    .await;
                    if let Some(pick) = &auto {
                        insert_header(out.headers_mut(), HEADER_AUTO_PICK, &pick.pick);
                        insert_header(out.headers_mut(), HEADER_AUTO_REASON, &pick.reason);
                    }
                    return Ok(out);
                }
                record_failure(
                    state,
                    model_id,
                    status,
                    started,
                    format!("HTTP {status}; retried on the next fallback"),
                    client.clone(),
                );
            }
            Err(reason) => {
                attempts_trail.push(format!("{}={reason}", model_id.qualified()));
                record_failure(state, model_id, 0, started, reason, client.clone());
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

/// The request in the forms an attempt can send it: translated and
/// transformed for a chat-completions upstream, or the caller's own Anthropic
/// body for an upstream that serves the model on `/v1/messages` natively.
struct AttemptBodies<'a> {
    original: &'a Bytes,
    json_body: Option<Value>,
    boundary: Option<String>,
    /// The caller's headers, without `anthropic-beta` on the Anthropic route.
    headers: Cow<'a, HeaderMap>,
    upstream_path: &'a str,
    force_identity_encoding: bool,
    route: RouteKind,
    plan: TransformPlan,
    /// The untranslated Anthropic body and its headers; `None` off the
    /// Anthropic route.
    native: Option<(Value, HeaderMap)>,
    /// A native response is already in the client's shape.
    passthrough_plan: TransformPlan,
}

/// What one attempt sends, and how its response is delivered.
struct Attempt<'a> {
    path: UpstreamPath<'a>,
    headers: &'a HeaderMap,
    body: Bytes,
    force_identity_encoding: bool,
    route: RouteKind,
    plan: &'a TransformPlan,
    /// A native Copilot `/v1/messages` attempt.
    native_messages: bool,
    reply_rewrite: ReplyRewrite,
}

/// The rewrite an attempt's raw upstream reply goes through before `deliver`.
enum ReplyRewrite {
    None,
    /// A Copilot Claude model on `/chat/completions`, which may answer in
    /// Anthropic shape; see [`repair_native_reply`].
    NativeReply,
    /// A streaming Copilot `/responses` attempt; see [`stabilize_item_ids`].
    StableItemIds,
    /// An Anthropic request bridged to a Responses-only Copilot model; see
    /// [`anthropic_from_responses`].
    AnthropicFromResponses {
        served_model: String,
        names: NameMap,
    },
}

impl ReplyRewrite {
    async fn apply(&self, response: reqwest::Response) -> reqwest::Response {
        match self {
            ReplyRewrite::None => response,
            ReplyRewrite::NativeReply => repair_native_reply(response),
            ReplyRewrite::StableItemIds => stabilize_item_ids(response),
            ReplyRewrite::AnthropicFromResponses {
                served_model,
                names,
            } => anthropic_from_responses(response, served_model, names).await,
        }
    }
}

impl<'a> AttemptBodies<'a> {
    /// Parses, translates and transforms the request once, before the chain
    /// runs, and returns it with the primary model.
    fn prepare(
        config: &crate::configs::HubConfig,
        path_and_query: &'a str,
        headers: &'a HeaderMap,
        body: &'a Bytes,
        route: RouteKind,
    ) -> Result<(AttemptBodies<'a>, ModelId), AppError> {
        let mut json_body: Option<Value> = if body.is_empty() {
            None
        } else {
            serde_json::from_slice(body).ok()
        };

        // A native `/v1/messages` attempt sends the client's own Anthropic body,
        // so keep it before translation replaces it.
        let native = json_body
            .clone()
            .filter(|_| route == RouteKind::Anthropic)
            .map(|anthropic_body| (anthropic_body, with_anthropic_version(headers)));

        // Translate first so everything downstream sees exactly one body shape.
        if route == RouteKind::Anthropic {
            json_body = Some(anthropic::request::to_openai(json_body.as_ref())?);
        }

        let boundary = multipart_boundary(headers);
        let primary = requested_model(json_body.as_ref(), body, boundary.as_deref());

        let Some(primary) = primary else {
            // No model in the body (e.g. GET without body): need a profile from the path? Reject clearly.
            return Err(AppError::BadRequest(
                "request body must contain a \"model\" field shaped <profile>/<model>".to_string(),
            ));
        };

        let primary_id = parse_model_or_400(config, &primary)?;
        inject_stream_usage(&mut json_body);

        // The single request-side transform seam. It runs once, before the chain is
        // built, so every attempt sends byte-identical bytes.
        let plan = transforms::apply_request(
            &mut json_body,
            headers,
            config.stream_role_inject,
            config.copilot_models(),
        );
        // Whenever the hub reads the response body itself it must be handed
        // uncompressed bytes; passthrough requests keep the caller's header.
        let force_identity_encoding =
            route == RouteKind::Anthropic || plan.wants_response_transform();

        // The Anthropic route always targets the upstream's chat-completions
        // endpoint, whatever the client-facing path was.
        let upstream_path = match route {
            RouteKind::OpenAi => path_and_query,
            RouteKind::Anthropic => ANTHROPIC_UPSTREAM_PATH,
        };

        let headers = if route == RouteKind::Anthropic {
            let mut translated = headers.clone();
            translated.remove(HEADER_ANTHROPIC_BETA);
            Cow::Owned(translated)
        } else {
            Cow::Borrowed(headers)
        };
        let bodies = AttemptBodies {
            original: body,
            json_body,
            boundary,
            headers,
            upstream_path,
            force_identity_encoding,
            route,
            plan,
            native,
            passthrough_plan: TransformPlan::default(),
        };
        Ok((bodies, primary_id))
    }

    fn for_model(&self, state: &AppState, model_id: &ModelId) -> Attempt<'_> {
        let catalog = state.upstream_model(&model_id.profile, &model_id.model);
        // Copilot lists Claude ids dotted, the hub advertises them dashed; an
        // id missing from the catalog is sent as the client wrote it.
        let sent_id = ModelId {
            profile: model_id.profile.clone(),
            model: catalog
                .as_ref()
                .map_or_else(|| model_id.model.clone(), |m| m.upstream_id.clone()),
        };
        let native = self
            .native
            .as_ref()
            .filter(|_| catalog.as_ref().is_some_and(|m| m.native_messages));
        let bridged = self
            .native
            .as_ref()
            .filter(|_| native.is_none() && serves_only_responses(state, &sent_id));
        if let Some((anthropic_body, _)) = bridged {
            return self.responses_attempt(anthropic_body, &sent_id, model_id);
        }
        let path = native_count_tokens_path(self.upstream_path, catalog.as_ref())
            .unwrap_or(UpstreamPath::Strip(self.upstream_path));
        let reply_rewrite = if may_reply_natively(state, self.upstream_path, &sent_id) {
            ReplyRewrite::NativeReply
        } else if streams_copilot_responses(state, path, self.json_body.as_ref(), &sent_id) {
            ReplyRewrite::StableItemIds
        } else {
            ReplyRewrite::None
        };
        match native {
            // The request transforms only ever touched the translated body,
            // and the response, error bodies included, goes back untouched.
            Some((anthropic_body, native_headers)) => Attempt {
                path: UpstreamPath::Verbatim(NATIVE_MESSAGES_PATH),
                headers: native_headers,
                body: json_for_attempt(
                    anthropic_body,
                    self.original,
                    &sent_id,
                    &state.unsupported_body_fields(&model_id.profile),
                ),
                // An "Extra inputs" 400 is read to learn the rejected field.
                force_identity_encoding: true,
                route: RouteKind::OpenAi,
                plan: &self.passthrough_plan,
                native_messages: true,
                reply_rewrite: ReplyRewrite::None,
            },
            None => Attempt {
                path,
                headers: &self.headers,
                body: body_for_attempt(
                    self.json_body.as_ref(),
                    self.original,
                    self.boundary.as_deref(),
                    &sent_id,
                ),
                // A rewrite reads the body, so it must arrive uncompressed.
                force_identity_encoding: self.force_identity_encoding
                    || !matches!(reply_rewrite, ReplyRewrite::None),
                route: self.route,
                plan: &self.plan,
                native_messages: false,
                reply_rewrite,
            },
        }
    }

    /// The Anthropic request as a Responses request, for a Copilot model that
    /// serves neither `/v1/messages` nor `/chat/completions`.
    fn responses_attempt(
        &self,
        anthropic_body: &Value,
        sent_id: &ModelId,
        model_id: &ModelId,
    ) -> Attempt<'_> {
        let (responses_body, names) = anthropic::responses_request::to_responses(anthropic_body);
        Attempt {
            path: UpstreamPath::Verbatim(UPSTREAM_RESPONSES_PATH),
            headers: &self.headers,
            body: json_for_attempt(&responses_body, self.original, sent_id, &[]),
            force_identity_encoding: true,
            route: RouteKind::OpenAi,
            plan: &self.passthrough_plan,
            native_messages: false,
            reply_rewrite: ReplyRewrite::AnthropicFromResponses {
                served_model: model_id.qualified(),
                names,
            },
        }
    }

    /// The upstream path and body Copilot Auto routes on. The Anthropic route
    /// offers its own body for `/v1/messages`, which a Claude pick serves natively.
    fn auto_input(&self) -> (&str, &Value) {
        match &self.native {
            Some((anthropic_body, _)) => (NATIVE_MESSAGES_PATH, anthropic_body),
            None => (
                UpstreamPath::Strip(self.upstream_path).as_str(),
                self.json_body.as_ref().unwrap_or(&NO_BODY),
            ),
        }
    }
}

/// The model sent in place of `<profile>/auto`, its Copilot session token,
/// and the values of the Auto response headers.
struct AutoPick {
    model_id: ModelId,
    session_token: Option<String>,
    /// Auto's own choice, as a qualified id.
    pick: String,
    /// Header-safe: the swap reason, else Auto's selection reason, else `auto`.
    reason: String,
}

/// Resolves `<profile>/auto` on a Copilot profile through Copilot Auto and
/// records the outcome in the router's decision log. Any other model returns
/// `None` before any lookup.
async fn resolve_auto(
    state: &AppState,
    bodies: &AttemptBodies<'_>,
    model_id: &ModelId,
) -> Option<AutoPick> {
    if model_id.model != COPILOT_AUTO_MODEL {
        return None;
    }
    let config = state.config();
    let profile = config.profile(&model_id.profile).filter(|p| p.enabled)?;
    let auth = profile.copilot.as_ref()?;
    let client = state.client(&profile.name)?;
    let (bearer, api_base) = match state
        .copilot_sessions()
        .get(&client, &profile.name, auth)
        .await
    {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!("profile {}: Copilot Auto has no session: {e}", profile.name);
            return None;
        }
    };
    let api_base = api_base.unwrap_or_else(|| profile.base_url.clone());
    let (path, body) = bodies.auto_input();
    let catalog = state.copilot_catalog(&profile.name);
    let route = state
        .copilot_auto()
        .route(
            &client,
            &profile.name,
            &bearer,
            &api_base,
            path,
            body,
            bodies.original.len(),
            &catalog,
            default_auto_model(&catalog, path),
        )
        .await;
    let model_id = ModelId {
        profile: profile.name.clone(),
        model: advertised_bare_id(profile, &route.model),
    };
    let auto_pick = ModelId {
        profile: profile.name.clone(),
        model: advertised_bare_id(profile, &route.auto_pick),
    }
    .qualified();
    let reason = route
        .swap_reason
        .as_deref()
        .or(route.selection_reason.as_deref())
        .unwrap_or("auto");
    let reason = header_safe(reason, AUTO_REASON_HEADER_MAX_CHARS);
    state.copilot_auto().decisions().record(AutoDecision {
        ts_ms: now_ms(),
        session: claude_code_session_id(&bodies.headers),
        profile: profile.name.clone(),
        auto_pick: auto_pick.clone(),
        model: model_id.qualified(),
        swap_reason: route.swap_reason,
        complexity: route.complexity,
        selection_reason: route.selection_reason,
        turn: route.turn,
        estimated_prompt_tokens: route.estimated_prompt_tokens,
    });
    Some(AutoPick {
        model_id,
        session_token: route.session_token,
        pick: auto_pick,
        reason,
    })
}

/// The caller's Claude Code session id, kept only when it is
/// `[A-Za-z0-9._-]{1,128}`, so the log and the decisions API never echo
/// arbitrary header bytes.
fn claude_code_session_id(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(HEADER_CLAUDE_CODE_SESSION_ID)?.as_bytes();
    let valid = (1..=CLAUDE_CODE_SESSION_ID_MAX_LEN).contains(&value.len())
        && value
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then(|| String::from_utf8_lossy(value).into_owned())
}

/// `text` cut to `max_chars` characters, with every character outside
/// printable ASCII (0x20-0x7E) replaced by `?`, so it is always a valid header value.
fn header_safe(text: &str, max_chars: usize) -> String {
    text.chars()
        .take(max_chars)
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect()
}

/// The model a failed Auto route falls back to: the first catalog model that
/// serves the endpoint of `path`.
fn default_auto_model<'c>(catalog: &'c [Value], path: &str) -> &'c str {
    let endpoint = path.split_once('?').map_or(path, |(bare, _)| bare);
    catalog
        .iter()
        .filter(|entry| {
            entry
                .get("supported_endpoints")
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|e| e.as_str() == Some(endpoint)))
        })
        .find_map(|entry| entry.get("id").and_then(Value::as_str))
        .unwrap_or(COPILOT_AUTO_MODEL)
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
    attempt: &Attempt<'_>,
    client: Option<String>,
) -> Response {
    let (route, plan, upstream_path) = (attempt.route, attempt.plan, attempt.path.as_str());
    let is_sse = is_event_stream(&response);
    // The Anthropic non-stream translation is the one body the hub buffers in
    // full; everything else streams through a `BodyTransform` (or, in the
    // common case, through nothing at all).
    let mut out = if route == RouteKind::Anthropic && !is_sse {
        build_anthropic_response(
            state,
            response,
            upstream_path,
            model_id,
            attempts_trail,
            started,
            plan,
            client,
        )
        .await
    } else {
        let transform = transforms::response_transform(route, plan, &model_id.qualified(), is_sse);
        build_response(
            state,
            response,
            upstream_path,
            model_id,
            attempts_trail,
            started,
            transform,
            client,
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
    let upstream_path = UpstreamPath::Strip(path_and_query);
    let response = send_attempt(
        state,
        &method,
        upstream_path,
        headers,
        body,
        model_id,
        attempt_timeout,
        false,
        None,
    )
    .await
    .map_err(AppError::UpstreamUnavailable)?;
    Ok(build_response(
        state,
        response,
        upstream_path.as_str(),
        model_id,
        &trail,
        started,
        None,
        client_signature(headers),
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

fn multipart_boundary(headers: &HeaderMap) -> Option<String> {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(multipart_model::boundary)
}

fn requested_model(
    json_body: Option<&Value>,
    body: &Bytes,
    multipart_boundary: Option<&str>,
) -> Option<String> {
    match multipart_boundary {
        Some(boundary) => multipart_model::model_field(body, boundary),
        None => json_body
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn body_for_attempt(
    json_body: Option<&Value>,
    original: &Bytes,
    multipart_boundary: Option<&str>,
    model_id: &ModelId,
) -> Bytes {
    if let Some(boundary) = multipart_boundary {
        return multipart_model::with_model_field(original, boundary, &model_id.model)
            .unwrap_or_else(|| original.clone());
    }
    let Some(json) = json_body else {
        return original.clone();
    };
    json_for_attempt(json, original, model_id, &[])
}

/// `json` sent to `model_id`, without the `rejected` fields.
fn json_for_attempt(
    json: &Value,
    original: &Bytes,
    model_id: &ModelId,
    rejected: &[BodyField],
) -> Bytes {
    let mut attempt = json.clone();
    if let Some(obj) = attempt.as_object_mut() {
        obj.insert("model".into(), Value::String(model_id.model.clone()));
    }
    for field in rejected {
        field.strip(&mut attempt);
    }
    Bytes::from(serde_json::to_vec(&attempt).unwrap_or_else(|_| original.to_vec()))
}

/// Sends one attempt's body, or a replacement for it, through `send_attempt`.
struct AttemptSender<'a> {
    state: &'a AppState,
    method: &'a Method,
    attempt: &'a Attempt<'a>,
    model_id: &'a ModelId,
    attempt_timeout: Option<Duration>,
    session_token: Option<&'a str>,
}

impl AttemptSender<'_> {
    async fn send(&self, body: Bytes) -> Result<reqwest::Response, String> {
        send_attempt(
            self.state,
            self.method,
            self.attempt.path,
            self.attempt.headers,
            body,
            self.model_id,
            self.attempt_timeout,
            self.attempt.force_identity_encoding,
            self.session_token,
        )
        .await
    }
}

/// Copilot rejects some Anthropic fields with "<path>: Extra inputs are not
/// permitted". Learns them, so later requests never send them, and retries once
/// without them. Any other response comes back unchanged.
async fn retry_without_rejected_fields(
    bodies: &AttemptBodies<'_>,
    sender: &AttemptSender<'_>,
    response: reqwest::Response,
) -> Result<reqwest::Response, String> {
    let compressed = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != IDENTITY_ENCODING);
    let large = response
        .content_length()
        .is_some_and(|length| length > MAX_REJECTION_BYTES);
    if response.status() != StatusCode::BAD_REQUEST || compressed || large {
        return Ok(response);
    }
    let (status, version, headers) = (
        response.status(),
        response.version(),
        response.headers().clone(),
    );
    // A chunked 400 has no length up front, so read it up to the cap.
    let mut chunks = response.bytes_stream();
    let mut read = Vec::new();
    while let Some(chunk) = chunks.next().await {
        read.extend_from_slice(&chunk.map_err(|e| format!("read error: {e}"))?);
        if read.len() as u64 > MAX_REJECTION_BYTES {
            let read = futures_util::stream::iter([Ok(Bytes::from(read))]);
            let body = reqwest::Body::wrap_stream(read.chain(chunks));
            return Ok(rebuilt_response(status, version, headers, body));
        }
    }
    let body = Bytes::from(read);
    let rejected = BodyField::rejected_in(&body);
    if rejected.is_empty() {
        return Ok(rebuilt_response(status, version, headers, body));
    }
    let (state, model_id) = (sender.state, sender.model_id);
    state.learn_unsupported_body_fields(&model_id.profile, &rejected);
    sender.send(bodies.for_model(state, model_id).body).await
}

/// A response whose body the hub already started reading, handed back whole.
fn rebuilt_response(
    status: StatusCode,
    version: reqwest::Version,
    headers: HeaderMap,
    body: impl Into<reqwest::Body>,
) -> reqwest::Response {
    let mut rebuilt = Response::new(body.into());
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    reqwest::Response::from(rebuilt)
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
    upstream_path: UpstreamPath<'_>,
    headers: &HeaderMap,
    body: Bytes,
    model_id: &ModelId,
    attempt_timeout: Option<Duration>,
    force_identity_encoding: bool,
    session_token: Option<&str>,
) -> Result<reqwest::Response, String> {
    let config = state.config();
    let profile = config
        .profile(&model_id.profile)
        .ok_or_else(|| format!("unknown profile: {}", model_id.profile))?;
    if !profile.enabled {
        return Err(format!("profile disabled: {}", profile.name));
    }
    let client = state.client(&profile.name).ok_or("no client for profile")?;
    state.managed().ensure_running(profile).await?;

    let upstream = UpstreamRequest {
        state,
        client: &client,
        profile,
        method,
        upstream_path: upstream_path.as_str(),
        headers,
        force_identity_encoding,
        timeout: attempt_timeout.or(profile.timeout_ms.map(Duration::from_millis)),
        session_token,
    };
    let (response, copilot_bearer) = upstream.send(body.clone()).await?;
    let rejected = upstream_auth::invalidate_if_unauthorized(
        state,
        &profile.name,
        response.status(),
        copilot_bearer.as_deref(),
    )
    .await;
    if !rejected {
        return Ok(response);
    }
    upstream.send(body).await.map(|(retried, _)| retried)
}

/// True for a Copilot Claude model sent to `/chat/completions`. Newer ones can
/// answer there in Anthropic shape, which [`repair_native_reply`] converts.
fn may_reply_natively(state: &AppState, upstream_path: &str, sent_id: &ModelId) -> bool {
    let bare_path = upstream_path
        .split_once('?')
        .map_or(upstream_path, |(path, _)| path);
    bare_path == ANTHROPIC_UPSTREAM_PATH
        && sent_id.model.starts_with(CLAUDE_MODEL_PREFIX)
        && state
            .config()
            .profile(&sent_id.profile)
            .is_some_and(|profile| profile.copilot.is_some())
}

/// Puts an Anthropic-shaped success reply into `OpenAI` chat-completion shape
/// (frame by frame for a stream) before anything else reads it. An
/// `OpenAI`-shaped reply, an error, or a compressed body is left alone.
fn repair_native_reply(response: reqwest::Response) -> reqwest::Response {
    if !is_rewritable(&response) {
        return response;
    }
    let repair = NativeReplyRepair::new(is_event_stream(&response));
    rewrite_body(response, repair)
}

/// True for a streaming request a Copilot profile sends to `/responses`.
fn streams_copilot_responses(
    state: &AppState,
    path: UpstreamPath<'_>,
    json_body: Option<&Value>,
    sent_id: &ModelId,
) -> bool {
    let upstream_path = path.as_str();
    let bare_path = upstream_path
        .split_once('?')
        .map_or(upstream_path, |(path, _)| path);
    bare_path == UPSTREAM_RESPONSES_PATH
        && json_body
            .and_then(|body| body.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && state
            .config()
            .profile(&sent_id.profile)
            .is_some_and(|profile| profile.copilot.is_some())
}

/// Pins one item id per `output_index` on a Copilot `/responses` stream, on the
/// raw upstream bytes, so every later rewrite or translation sees stable ids.
fn stabilize_item_ids(response: reqwest::Response) -> reqwest::Response {
    if !is_rewritable(&response) || !is_event_stream(&response) {
        return response;
    }
    rewrite_body(response, ResponsesItemIds::new())
}

/// True when the Copilot catalog lists `sent_id` on `/responses` and on neither
/// `/chat/completions` nor `/v1/messages`.
fn serves_only_responses(state: &AppState, sent_id: &ModelId) -> bool {
    let catalog = state.copilot_catalog(&sent_id.profile);
    let Some(endpoints) = catalog
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(sent_id.model.as_str()))
        .and_then(|entry| entry.get("supported_endpoints"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let serves = |wanted: &str| endpoints.iter().any(|e| e.as_str() == Some(wanted));
    serves(UPSTREAM_RESPONSES_PATH)
        && !serves(CHAT_COMPLETIONS_ENDPOINT)
        && !serves(NATIVE_MESSAGES_PATH)
}

/// Puts a successful Copilot `/responses` reply into Anthropic Messages shape:
/// a stream frame by frame once its item ids are pinned, a JSON body whole. An
/// error or a compressed body is left alone, so prompt-limit learning reads it.
async fn anthropic_from_responses(
    response: reqwest::Response,
    served_model: &str,
    names: &NameMap,
) -> reqwest::Response {
    if !is_rewritable(&response) {
        return response;
    }
    if is_event_stream(&response) {
        let translator = ResponsesStream::new(served_model.to_string(), names.clone());
        return rewrite_body(stabilize_item_ids(response), translator);
    }
    let (status, version) = (response.status(), response.version());
    let mut headers = response.headers().clone();
    headers.remove(reqwest::header::CONTENT_LENGTH);
    let (raw, overflowed) = read_upstream_capped(response, MAX_TRANSFORM_BUFFER_BYTES).await;
    if overflowed {
        let body = anthropic::response::error_body(
            StatusCode::BAD_GATEWAY.as_u16(),
            "upstream response is too large to translate into the anthropic message shape",
        );
        return rebuilt_response(StatusCode::BAD_GATEWAY, version, headers, body);
    }
    let body = anthropic::responses_message::to_anthropic(&raw, served_model, names);
    rebuilt_response(status, version, headers, body)
}

/// A successful, uncompressed reply, whose bytes the hub can rewrite.
fn is_rewritable(response: &reqwest::Response) -> bool {
    let compressed = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != IDENTITY_ENCODING);
    response.status().is_success() && !compressed
}

/// A chunk-by-chunk rewrite of an upstream body, run before `deliver` reads it.
trait UpstreamRewrite: Send + 'static {
    /// Feeds upstream bytes and returns what is ready to send on.
    fn push(&mut self, bytes: &[u8]) -> Vec<u8>;
    /// Returns whatever is still held at the end of the body.
    fn finish(&mut self) -> Vec<u8>;
}

impl UpstreamRewrite for NativeReplyRepair {
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        NativeReplyRepair::push(self, bytes)
    }

    fn finish(&mut self) -> Vec<u8> {
        NativeReplyRepair::finish(self)
    }
}

impl UpstreamRewrite for ResponsesStream {
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        ResponsesStream::push(self, bytes)
    }

    fn finish(&mut self) -> Vec<u8> {
        ResponsesStream::finish(self)
    }
}

impl UpstreamRewrite for ResponsesItemIds {
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        ResponsesItemIds::push(self, bytes)
    }

    fn finish(&mut self) -> Vec<u8> {
        ResponsesItemIds::finish(self)
    }
}

/// Rebuilds `response` with its body streamed through `rewrite`; status,
/// version and headers carry over, except the now-wrong `content-length`.
fn rewrite_body(response: reqwest::Response, rewrite: impl UpstreamRewrite) -> reqwest::Response {
    let (status, version) = (response.status(), response.version());
    let mut headers = response.headers().clone();
    headers.remove(reqwest::header::CONTENT_LENGTH);
    let rewritten = futures_util::stream::unfold(
        Some((response.bytes_stream(), rewrite)),
        |step| async move {
            let (mut inner, mut rewrite) = step?;
            loop {
                match inner.next().await {
                    Some(Ok(bytes)) => {
                        let out = rewrite.push(&bytes);
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), Some((inner, rewrite))));
                        }
                    }
                    Some(Err(e)) => return Some((Err(e), None)),
                    None => {
                        let tail = rewrite.finish();
                        return (!tail.is_empty()).then(|| (Ok(Bytes::from(tail)), None));
                    }
                }
            }
        },
    );
    let mut rebuilt = Response::new(reqwest::Body::wrap_stream(rewritten));
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    reqwest::Response::from(rebuilt)
}

/// Copilot serves Anthropic token counting at `/v1/messages/count_tokens`, so a
/// native Claude model keeps the `/v1` that `Strip` would drop.
fn native_count_tokens_path(
    client_path: &str,
    catalog: Option<&UpstreamModel>,
) -> Option<UpstreamPath<'static>> {
    let bare_path = client_path
        .split_once('?')
        .map_or(client_path, |(path, _)| path);
    let is_count_tokens = bare_path == NATIVE_COUNT_TOKENS_PATH;
    (is_count_tokens && catalog.is_some_and(|model| model.native_messages))
        .then_some(UpstreamPath::Verbatim(NATIVE_COUNT_TOKENS_PATH))
}

/// The path an attempt is sent to, appended to the profile's base URL.
#[derive(Clone, Copy)]
enum UpstreamPath<'a> {
    /// A client-facing `/v1/...` path; upstream bases already end at the API
    /// root, so the `/v1` is dropped.
    Strip(&'a str),
    /// An upstream path that keeps its `/v1`, such as Copilot's `/v1/messages`.
    Verbatim(&'static str),
}

impl<'a> UpstreamPath<'a> {
    fn as_str(self) -> &'a str {
        match self {
            UpstreamPath::Strip(path) => path.strip_prefix("/v1").unwrap_or(path),
            UpstreamPath::Verbatim(path) => path,
        }
    }
}

/// The `/v1/messages` route strips the caller's `anthropic-*` headers before
/// the attempt loop, but the Anthropic API requires a version, so a native
/// attempt sends the default one when the caller's is gone.
fn with_anthropic_version(headers: &HeaderMap) -> HeaderMap {
    let mut native = headers.clone();
    if !native.contains_key(HEADER_ANTHROPIC_VERSION) {
        native.insert(
            HEADER_ANTHROPIC_VERSION,
            HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
        );
    }
    native
}

/// One upstream call of `send_attempt`, kept whole so a Copilot 401 can be
/// re-sent with fresh credentials.
struct UpstreamRequest<'a> {
    state: &'a AppState,
    client: &'a reqwest::Client,
    profile: &'a ProfileConfig,
    method: &'a Method,
    upstream_path: &'a str,
    headers: &'a HeaderMap,
    force_identity_encoding: bool,
    timeout: Option<Duration>,
    /// Copilot Auto's `Copilot-Session-Token`, on `<profile>/auto` requests only.
    session_token: Option<&'a str>,
}

impl UpstreamRequest<'_> {
    /// Returns the response and the Copilot bearer it was sent with. The
    /// timeout covers the credential lookup as well as the upstream call.
    async fn send(&self, body: Bytes) -> Result<(reqwest::Response, Option<String>), String> {
        let send = self.resolve_and_send(body);
        let result = match self.timeout {
            Some(timeout) => tokio::time::timeout(timeout, send)
                .await
                .map_err(|_| "timeout".to_string())?,
            None => send.await,
        };
        self.state.managed().touch(&self.profile.name);
        result
    }

    async fn resolve_and_send(
        &self,
        body: Bytes,
    ) -> Result<(reqwest::Response, Option<String>), String> {
        let mut forwarded = filter_request_headers(self.headers);
        if self.force_identity_encoding {
            forwarded.insert("accept-encoding", HeaderValue::from_static("identity"));
        }
        if let Some(token) = self.session_token {
            let mut value = HeaderValue::from_str(token).map_err(|_| {
                "the Copilot Auto session token is not a valid header value".to_string()
            })?;
            value.set_sensitive(true);
            forwarded.insert(HEADER_COPILOT_SESSION_TOKEN, value);
        }
        let (base, copilot_bearer) = upstream_auth::resolve(
            self.state,
            self.client,
            self.profile,
            self.upstream_path,
            &body,
            &mut forwarded,
        )
        .await?;
        let mut request = self
            .client
            .request(self.method.clone(), format!("{base}{}", self.upstream_path))
            .headers(to_reqwest_headers(&forwarded));
        for (name, value) in &self.profile.extra_headers {
            request = request.header(name, value);
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("connect error: {e}"))?;
        Ok((response, copilot_bearer))
    }
}

/// Streams the upstream response through, observing a rolling tail for usage
/// scraping and recording stats when the stream ends.
///
/// `transform` is the only thing that decides whether the bytes are touched.
/// `None` — every route but the Anthropic one — is byte-for-byte passthrough
/// with no copy and no framing. `Some` rewrites frame by frame, and its
/// `finish()` output is emitted as a final chunk so the client is always left
/// with a well-formed body, even when the upstream is cut mid-stream.
// One argument over the lint's threshold, same trade-off as `send_attempt`.
#[allow(clippy::too_many_arguments)]
fn build_response(
    state: &AppState,
    upstream: reqwest::Response,
    upstream_path: &str,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    transform: Option<BodyTransform>,
    client: Option<String>,
) -> Response {
    let status = upstream.status();
    let limit_model =
        prompt_limit_model(state, status, upstream.headers(), model_id, upstream_path);
    // Read before `bytes_stream()` consumes the response. A comment frame
    // injected into a JSON body would corrupt it, so this is SSE-only.
    let keepalive = is_event_stream(&upstream)
        .then(|| state.config().sse_keepalive)
        .flatten();
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
        ts_ms: 0,
        latency_ms: 0,
        ttft_ms: None,
        tokens_in: 0,
        tokens_out: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        error: None,
        client,
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
            keepalive,
        },
        pump,
    );

    tokio::spawn(async move {
        let summary = summary_receiver.await.unwrap_or_default();
        if let Some(model) = &limit_model {
            state_for_stats.learn_prompt_limit(&outcome_seed.profile, model, &summary.tail);
        }
        let scraped = scrape_usage(&summary.tail);
        let usage = match summary.observed_usage {
            Some(observed) => ScrapedUsage {
                tokens_in: observed.tokens_in,
                tokens_out: observed.tokens_out,
                cache_read_tokens: observed.cache_read_tokens.max(scraped.cache_read_tokens),
                cache_write_tokens: observed.cache_write_tokens.max(scraped.cache_write_tokens),
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
            error: (outcome_seed.status >= ERROR_STATUS_MIN)
                .then(|| error_reason(outcome_seed.status, &summary.tail)),
            ..outcome_seed
        };
        log_failure(&outcome);
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
            match next_chunk(&mut state.inner, state.keepalive).await {
                // Nothing from upstream for a whole interval. The frame goes
                // out ahead of the transform and without touching `tail` or
                // `ttft_ms`: it is the hub's own byte, not the upstream's, and
                // must not show up in usage scraping or latency stats.
                NextChunk::Idle => {
                    let ping = Bytes::from_static(SSE_KEEPALIVE_FRAME.as_bytes());
                    return Some((Ok(ping), state));
                }
                NextChunk::Chunk(Ok(bytes)) => {
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
                NextChunk::Chunk(Err(e)) if state.transform.is_none() => {
                    return Some((Err(e), state));
                }
                NextChunk::Chunk(Err(e)) => {
                    tracing::warn!("upstream stream error, closing translated stream: {e}");
                    state.ended = true;
                }
                NextChunk::End => state.ended = true,
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

/// What one poll of the upstream body produced.
enum NextChunk {
    /// The keepalive interval elapsed with the upstream still silent.
    Idle,
    Chunk(reqwest::Result<Bytes>),
    End,
}

/// Awaits the next upstream chunk, giving up after `keepalive` so the caller
/// can send a comment frame and come back.
///
/// Cancelling the `next()` future on timeout loses nothing: `Stream::poll_next`
/// only yields an item by returning `Ready`, so a future dropped while `Pending`
/// has consumed nothing. Without `keepalive` this is a plain `next().await`.
async fn next_chunk<S>(inner: &mut S, keepalive: Option<Duration>) -> NextChunk
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let Some(interval) = keepalive else {
        return match inner.next().await {
            Some(item) => NextChunk::Chunk(item),
            None => NextChunk::End,
        };
    };
    match tokio::time::timeout(interval, inner.next()).await {
        Err(_elapsed) => NextChunk::Idle,
        Ok(Some(item)) => NextChunk::Chunk(item),
        Ok(None) => NextChunk::End,
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
    /// Idle gap after which a keepalive comment goes out. `None` on every
    /// non-SSE body and whenever the knob is off — see `pump`.
    keepalive: Option<Duration>,
}

/// Clients such as Copilot CLI and Claude Code close the connection as soon as
/// they read the final SSE frame, so the body can be dropped before `pump`
/// sees upstream EOF. Hand the stats task what was observed so far instead of
/// letting the sender drop — which recorded 0 tokens and no TTFT.
impl<S> Drop for StreamState<S> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(StreamSummary {
                tail: std::mem::take(&mut self.tail),
                observed_usage: self
                    .transform
                    .as_ref()
                    .and_then(BodyTransform::observed_usage),
                ttft_ms: self.ttft_ms,
            });
        }
    }
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
// One argument over the lint's threshold, same trade-off as `send_attempt`.
#[allow(clippy::too_many_arguments)]
async fn build_anthropic_response(
    state: &AppState,
    upstream: reqwest::Response,
    upstream_path: &str,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    plan: &TransformPlan,
    client: Option<String>,
) -> Response {
    let upstream_status = upstream.status();
    let limit_model = prompt_limit_model(
        state,
        upstream_status,
        upstream.headers(),
        model_id,
        upstream_path,
    );
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
    if let Some(model) = &limit_model {
        state.learn_prompt_limit(&model_id.profile, model, &raw);
    }
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
        ts_ms: 0,
        latency_ms,
        // Buffered replies have no first-token event; treat full latency as TTFT.
        ttft_ms: Some(latency_ms),
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        error: (status.as_u16() >= ERROR_STATUS_MIN).then(|| {
            if overflowed {
                "upstream response too large to translate".to_string()
            } else {
                error_reason(status.as_u16(), &raw)
            }
        }),
        client,
    };
    log_failure(&outcome);
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

/// The upstream model id a Copilot 400 on `upstream_path` may teach a prompt
/// limit for, read before the body streams. A compressed body is skipped: the
/// hub sees its raw bytes.
fn prompt_limit_model(
    state: &AppState,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    model_id: &ModelId,
    upstream_path: &str,
) -> Option<String> {
    if status != StatusCode::BAD_REQUEST {
        return None;
    }
    let compressed = headers
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != IDENTITY_ENCODING);
    if compressed {
        return None;
    }
    state
        .config()
        .profile(&model_id.profile)?
        .copilot
        .as_ref()?;
    let upstream = state.upstream_model(&model_id.profile, &model_id.model);
    let upstream_id = upstream.map_or_else(|| model_id.model.clone(), |entry| entry.upstream_id);
    teaches_stored_limit(upstream_path, &upstream_id).then_some(upstream_id)
}

/// True when a rejection on `upstream_path` speaks for the limit stored for
/// `upstream_id`. A Claude limit describes native `/v1/messages`, so only that
/// endpoint and its `count_tokens` teach it; any other model learns from `/responses`.
fn teaches_stored_limit(upstream_path: &str, upstream_id: &str) -> bool {
    let bare_path = upstream_path
        .split_once('?')
        .map_or(upstream_path, |(path, _)| path);
    if upstream_id.starts_with(CLAUDE_MODEL_PREFIX) {
        bare_path == NATIVE_MESSAGES_PATH || bare_path == NATIVE_COUNT_TOKENS_PATH
    } else {
        bare_path == UPSTREAM_RESPONSES_PATH
    }
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

fn record_failure(
    state: &AppState,
    model_id: &ModelId,
    status: u16,
    started: Instant,
    reason: String,
    client: Option<String>,
) {
    let latency_ms = crate::utils::time::elapsed_ms(started);
    let outcome = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key: model_id.qualified(),
        status: if status == 0 { 599 } else { status },
        ts_ms: 0,
        latency_ms,
        ttft_ms: None,
        tokens_in: 0,
        tokens_out: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        error: Some(reason),
        client,
    };
    log_failure(&outcome);
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
    state
        .managed()
        .ensure_running(profile)
        .await
        .map_err(AppError::UpstreamUnavailable)?;

    let upstream_path = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
    let mut forwarded = filter_request_headers(headers);
    let (base, copilot_bearer) = upstream_auth::resolve(
        state,
        &client,
        profile,
        upstream_path,
        &buffered,
        &mut forwarded,
    )
    .await
    .map_err(AppError::UpstreamUnavailable)?;
    let url = format!("{base}{upstream_path}");

    let combined = futures_util::stream::iter(vec![Ok::<Bytes, axum::Error>(buffered)]).chain(rest);
    let mut request = client
        .request(method, url)
        .headers(to_reqwest_headers(&forwarded))
        .body(reqwest::Body::wrap_stream(combined));
    for (name, value) in &profile.extra_headers {
        request = request.header(name, value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| AppError::UpstreamUnavailable(e.to_string()))?;
    // A streamed body cannot be replayed, so a rejected Copilot session is only dropped.
    upstream_auth::invalidate_if_unauthorized(
        state,
        &profile.name,
        response.status(),
        copilot_bearer.as_deref(),
    )
    .await;
    let started = Instant::now();
    Ok(build_response(
        state,
        response,
        upstream_path,
        &model_id,
        &[model_id.qualified()],
        started,
        None,
        client_signature(headers),
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

/// Short, human-readable reason for a failed upstream body: the JSON
/// `error.message` / `message` / `detail` when present, else the raw text.
fn error_reason(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    let from_json = serde_json::from_str::<Value>(trimmed).ok().and_then(|v| {
        let error = v.get("error");
        error
            .and_then(|e| e.get("message"))
            .or(error)
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .and_then(|m| match m {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string()),
            })
    });
    let reason = from_json.unwrap_or_else(|| trimmed.to_string());
    let reason = if reason.is_empty() {
        // e.g. a backend with no handler for the path (the MLX proxy used to
        // 404 every /v1/chat/completions this way) -- the status is all there is.
        format!("HTTP {status} with empty body from upstream")
    } else {
        reason
    };
    reason.chars().take(ERROR_REASON_MAX_CHARS).collect()
}

fn log_failure(outcome: &RequestOutcome) {
    if let Some(reason) = &outcome.error {
        tracing::warn!(
            model = %outcome.model_key,
            status = outcome.status,
            latency_ms = outcome.latency_ms,
            "request failed: {reason}"
        );
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::stream::{self, BoxStream, StreamExt as _};

    use super::*;

    const KEEPALIVE: Duration = Duration::from_millis(25);
    /// Comfortably longer than `KEEPALIVE`, so the idle branch fires at least
    /// twice without the test depending on exact scheduler timing.
    const SILENCE_MS: u64 = 120;

    type TestStream = BoxStream<'static, reqwest::Result<Bytes>>;

    #[test]
    fn count_tokens_on_native_claude_model_keeps_v1_prefix() {
        let native = UpstreamModel {
            upstream_id: "claude-opus-5.5".into(),
            native_messages: true,
        };
        let translated = UpstreamModel {
            upstream_id: "gpt-6-luna".into(),
            native_messages: false,
        };

        let routed = native_count_tokens_path("/v1/messages/count_tokens?beta=true", Some(&native));

        assert_eq!(
            routed.map(UpstreamPath::as_str),
            Some(NATIVE_COUNT_TOKENS_PATH)
        );
        assert!(native_count_tokens_path("/v1/messages/count_tokens", Some(&translated)).is_none());
        assert!(native_count_tokens_path("/v1/chat/completions", Some(&native)).is_none());
        assert!(native_count_tokens_path("/v1/messages/count_tokens", None).is_none());
    }

    /// Upstream that sleeps `delay_ms` before handing over each payload, which
    /// is what gives `pump` a real idle gap to react to.
    fn slow_stream(steps: Vec<(u64, &'static str)>) -> TestStream {
        stream::unfold(steps.into_iter(), |mut steps| async move {
            let (delay_ms, payload) = steps.next()?;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            Some((Ok(Bytes::from_static(payload.as_bytes())), steps))
        })
        .boxed()
    }

    fn state_for(
        inner: TestStream,
        keepalive: Option<Duration>,
        sender: Option<tokio::sync::oneshot::Sender<StreamSummary>>,
    ) -> StreamState<TestStream> {
        StreamState {
            inner,
            tail: Vec::new(),
            transform: None,
            sender,
            ended: false,
            started: Instant::now(),
            ttft_ms: None,
            keepalive,
        }
    }

    /// Drives the body stream to exhaustion, returning every chunk as text.
    async fn drain(mut state: StreamState<TestStream>) -> Vec<String> {
        let mut chunks = Vec::new();
        while let Some((item, next)) = pump(state).await {
            chunks.push(String::from_utf8(item.unwrap().to_vec()).unwrap());
            state = next;
        }
        chunks
    }

    fn ping_count(chunks: &[String]) -> usize {
        chunks.iter().filter(|c| *c == SSE_KEEPALIVE_FRAME).count()
    }

    #[tokio::test]
    async fn pings_while_the_upstream_is_silent_then_stops_once_bytes_flow() {
        let upstream = slow_stream(vec![(SILENCE_MS, "data: one\n\n"), (0, "data: two\n\n")]);
        let chunks = drain(state_for(upstream, Some(KEEPALIVE), None)).await;

        assert!(ping_count(&chunks) >= 2, "{chunks:?}");
        let payloads: Vec<&String> = chunks
            .iter()
            .filter(|c| *c != SSE_KEEPALIVE_FRAME)
            .collect();
        assert_eq!(payloads, vec!["data: one\n\n", "data: two\n\n"]);
        // The second payload followed the first with no delay, so no ping can
        // separate them.
        let first = chunks.iter().position(|c| c == "data: one\n\n").unwrap();
        assert_eq!(chunks[first + 1], "data: two\n\n", "{chunks:?}");
    }

    #[tokio::test]
    async fn client_closing_before_upstream_eof_still_reports_usage_and_ttft() {
        let usage_frame = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":34}}\n\ndata: [DONE]\n\n";
        // The upstream never reaches EOF while the client is reading.
        let upstream = stream::iter(vec![Ok(Bytes::from(usage_frame))])
            .chain(stream::pending())
            .boxed();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let state = state_for(upstream, None, Some(sender));

        let (first, state) = pump(state).await.unwrap();
        assert_eq!(first.unwrap(), Bytes::from(usage_frame));
        drop(state);

        let summary = receiver.await.expect("summary must be sent on drop");
        assert!(summary.ttft_ms.is_some());
        let usage = scrape_usage(&summary.tail);
        assert_eq!((usage.tokens_in, usage.tokens_out), (1200, 34));
    }

    #[test]
    fn error_reason_prefers_the_json_message() {
        let body = br#"{"error":{"code":400,"message":"request (69657 tokens) exceeds the available context size (65536 tokens)","type":"exceed_context_size_error"}}"#;
        assert_eq!(
            error_reason(400, body),
            "request (69657 tokens) exceeds the available context size (65536 tokens)"
        );
        assert_eq!(error_reason(400, br#"{"error":"bad model"}"#), "bad model");
        assert_eq!(
            error_reason(500, b"  upstream exploded \n"),
            "upstream exploded"
        );
        assert_eq!(
            error_reason(404, b""),
            "HTTP 404 with empty body from upstream"
        );
        assert_eq!(
            error_reason(400, "x".repeat(2000).as_bytes()).len(),
            ERROR_REASON_MAX_CHARS
        );
    }

    #[tokio::test]
    async fn no_ping_after_the_stream_ends() {
        let upstream = slow_stream(vec![(0, "data: only\n\n")]);
        let chunks = drain(state_for(upstream, Some(KEEPALIVE), None)).await;
        assert_eq!(chunks, vec!["data: only\n\n"]);
    }

    #[tokio::test]
    async fn a_disabled_keepalive_never_pings_however_long_the_silence() {
        let upstream = slow_stream(vec![(SILENCE_MS, "data: one\n\n")]);
        let chunks = drain(state_for(upstream, None, None)).await;
        assert_eq!(chunks, vec!["data: one\n\n"]);
    }

    #[tokio::test]
    async fn ping_bytes_reach_the_client_but_not_the_stats() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
        let upstream = slow_stream(vec![(SILENCE_MS, "data: real\n\n")]);
        let chunks = drain(state_for(upstream, Some(KEEPALIVE), Some(sender))).await;
        assert!(ping_count(&chunks) >= 2, "{chunks:?}");

        let summary = receiver.await.unwrap();
        assert_eq!(String::from_utf8(summary.tail).unwrap(), "data: real\n\n");
        // The clock starts at the real first byte, not at the first ping.
        let ttft = summary.ttft_ms.unwrap();
        assert!(ttft >= SILENCE_MS, "ttft {ttft}ms measured from a ping");
    }

    mod copilot_routing {
        use std::collections::HashMap;
        use std::sync::Arc;

        use serde_json::json;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        use super::*;
        use crate::configs::{CopilotFlow, HubConfig};
        use crate::consts::HEADER_FALLBACKS;
        use crate::schemas::upstream_model::UpstreamModel;
        use crate::services::copilot_auto::AutoTurn;
        use crate::services::credential_store::CredentialStore;
        use crate::services::fake_gh_cli::FakeGhCli;
        use crate::services::gh_cli::GhOutcome;
        use crate::services::memory_credential_store::MemoryCredentialStore;

        const PROFILE: &str = "copilot";
        const NATIVE_ID: &str = "claude-opus-4-5";
        const NATIVE_UPSTREAM_ID: &str = "claude-opus-4.5";
        const TRANSLATED_ID: &str = "claude-sonnet-4-5";
        const TRANSLATED_UPSTREAM_ID: &str = "claude-sonnet-4.5";
        const PLAIN_ID: &str = "gpt-4.1";
        const UPSTREAM_CHAT_PATH: &str = "/chat/completions";
        const CALLER_ANTHROPIC_VERSION: &str = "2023-01-01";

        struct Copilot {
            state: AppState,
            upstream: MockServer,
            // Kept alive for the session lookup.
            _github: MockServer,
        }

        async fn copilot() -> Copilot {
            let github = MockServer::start().await;
            let upstream = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/copilot_internal/user"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "chat_enabled": true,
                    "endpoints": {"api": upstream.uri()},
                })))
                .mount(&github)
                .await;
            let vars: HashMap<String, String> = [
                ("LLM_HUB_PROFILES", PROFILE.to_string()),
                ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9".to_string()),
                (
                    "LLM_HUB_COPILOT_AUTH",
                    CopilotFlow::Opencode.auth_value().to_string(),
                ),
                ("LLM_HUB_COPILOT_GITHUB_API_URL", github.uri()),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
            let store = MemoryCredentialStore::default();
            store
                .set(
                    &CopilotFlow::Opencode.credential_account("github.com"),
                    "gho_login",
                )
                .unwrap();
            let gh = FakeGhCli::new(GhOutcome::Token("gho_gh".to_string()));
            let state = AppState::with_seams(
                HubConfig::from_map(&vars).unwrap(),
                None,
                Arc::new(store),
                Arc::new(gh),
            )
            .unwrap();
            state.set_upstream_models(
                PROFILE,
                HashMap::from([
                    (NATIVE_ID.to_string(), entry(NATIVE_UPSTREAM_ID, true)),
                    (
                        TRANSLATED_ID.to_string(),
                        entry(TRANSLATED_UPSTREAM_ID, false),
                    ),
                    (PLAIN_ID.to_string(), entry(PLAIN_ID, false)),
                ]),
            );
            Copilot {
                state,
                upstream,
                _github: github,
            }
        }

        fn entry(upstream_id: &str, native_messages: bool) -> UpstreamModel {
            UpstreamModel {
                upstream_id: upstream_id.to_string(),
                native_messages,
            }
        }

        fn anthropic_body(model: &str, stream: bool) -> Value {
            json!({
                "model": format!("{PROFILE}/{model}"),
                "max_tokens": 64,
                "stream": stream,
                "thinking": {"type": "enabled", "budget_tokens": 1024},
                "messages": [{"role": "user", "content": "hi"}],
            })
        }

        fn client_headers() -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers
        }

        fn with_caller_version(mut headers: HeaderMap) -> HeaderMap {
            headers.insert(
                HEADER_ANTHROPIC_VERSION,
                HeaderValue::from_static(CALLER_ANTHROPIC_VERSION),
            );
            headers
        }

        fn chat_completion() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "translated"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1},
            }))
        }

        async fn send(
            copilot: &Copilot,
            headers: &HeaderMap,
            body: &Value,
            route: RouteKind,
        ) -> (u16, Bytes) {
            let path = match route {
                RouteKind::Anthropic => NATIVE_MESSAGES_PATH,
                RouteKind::OpenAi => "/v1/chat/completions",
            };
            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                path,
                headers,
                Bytes::from(body.to_string()),
                route,
            )
            .await
            .unwrap();
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, bytes)
        }

        /// Like `send`, for the response headers instead of the body.
        async fn send_for_headers(
            copilot: &Copilot,
            headers: &HeaderMap,
            body: &Value,
            route: RouteKind,
        ) -> HeaderMap {
            let path = match route {
                RouteKind::Anthropic => NATIVE_MESSAGES_PATH,
                RouteKind::OpenAi => "/v1/chat/completions",
            };
            attempt_loop(
                &copilot.state,
                Method::POST,
                path,
                headers,
                Bytes::from(body.to_string()),
                route,
            )
            .await
            .unwrap()
            .headers()
            .clone()
        }

        fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
            headers.get(name).map(|value| value.to_str().unwrap())
        }

        async fn requests_to(copilot: &Copilot, wanted: &str) -> Vec<Request> {
            copilot
                .upstream
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| request.url.path() == wanted)
                .collect()
        }

        fn sent_json(request: &Request) -> Value {
            serde_json::from_slice(&request.body).unwrap()
        }

        #[tokio::test]
        async fn native_claude_model_gets_the_original_body_dotted_on_v1_messages() {
            let copilot = copilot().await;
            let reply = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"native"}],"usage":{"input_tokens":3,"output_tokens":1}}"#;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(reply.as_bytes(), "application/json"),
                )
                .mount(&copilot.upstream)
                .await;
            let body = anthropic_body(NATIVE_ID, false);

            let (status, bytes) = send(
                &copilot,
                &with_caller_version(client_headers()),
                &body,
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(bytes, reply.as_bytes());
            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            assert_eq!(sent.len(), 1);
            let mut expected = body.clone();
            expected["model"] = json!(NATIVE_UPSTREAM_ID);
            assert_eq!(sent_json(&sent[0]), expected);
            assert_eq!(
                sent[0].headers.get(HEADER_ANTHROPIC_VERSION).unwrap(),
                CALLER_ANTHROPIC_VERSION
            );
            assert!(requests_to(&copilot, UPSTREAM_CHAT_PATH).await.is_empty());
        }

        #[tokio::test]
        async fn native_attempt_sends_the_default_version_when_the_caller_sent_none() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .mount(&copilot.upstream)
                .await;

            send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            assert_eq!(
                sent[0].headers.get(HEADER_ANTHROPIC_VERSION).unwrap(),
                DEFAULT_ANTHROPIC_VERSION
            );
        }

        #[tokio::test]
        async fn native_stream_passes_through_untouched() {
            let copilot = copilot().await;
            let frames = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(frames.as_bytes(), "text/event-stream"),
                )
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, true),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(bytes, frames.as_bytes());
            let sent = sent_json(&requests_to(&copilot, NATIVE_MESSAGES_PATH).await[0]);
            assert!(sent.get("stream_options").is_none(), "{sent}");
        }

        fn prompt_too_long() -> ResponseTemplate {
            let error = json!({"error": {
                "code": "model_max_prompt_tokens_exceeded",
                "message": "prompt is too long: 1200000 tokens > 900000 maximum",
            }});
            ResponseTemplate::new(400).set_body_json(error)
        }

        fn learned_limit(copilot: &Copilot, upstream_id: &str) -> Option<u64> {
            let limits = copilot.state.model_limits().for_profile(PROFILE);
            limits.get(upstream_id).map(|entry| entry.limit)
        }

        #[tokio::test]
        async fn native_prompt_too_long_rejection_teaches_the_limit() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(prompt_too_long())
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, true),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 400);
            // The streamed path learns in its stats task, after the body ends.
            for _ in 0..100 {
                if learned_limit(&copilot, NATIVE_UPSTREAM_ID).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(learned_limit(&copilot, NATIVE_UPSTREAM_ID), Some(900_000));
        }

        #[tokio::test]
        async fn chat_completions_rejection_keeps_the_claude_seed() {
            const SEEDED_ID: &str = "claude-opus-5.5";
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(prompt_too_long())
                .mount(&copilot.upstream)
                .await;

            // Off the catalog, so it is translated to `/chat/completions`.
            let (status, _) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(SEEDED_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 400);
            assert_eq!(requests_to(&copilot, UPSTREAM_CHAT_PATH).await.len(), 1);
            assert_eq!(learned_limit(&copilot, SEEDED_ID), Some(1_000_000));
        }

        #[tokio::test]
        async fn responses_rejection_teaches_a_non_claude_limit() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .respond_with(prompt_too_long())
                .mount(&copilot.upstream)
                .await;
            let body = json!({"model": format!("{PROFILE}/{PLAIN_ID}"), "input": "hi"});

            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                "/v1/responses",
                &client_headers(),
                Bytes::from(body.to_string()),
                RouteKind::OpenAi,
            )
            .await
            .unwrap();
            let status = response.status().as_u16();
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();

            assert_eq!(status, 400);
            // The streamed path learns in its stats task, after the body ends.
            for _ in 0..100 {
                if learned_limit(&copilot, PLAIN_ID).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(learned_limit(&copilot, PLAIN_ID), Some(900_000));
        }

        #[tokio::test]
        async fn native_error_body_is_forwarded_unmodified() {
            let copilot = copilot().await;
            let error = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking is not supported"}}"#;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(
                    ResponseTemplate::new(400).set_body_raw(error.as_bytes(), "application/json"),
                )
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 400);
            assert_eq!(bytes, error.as_bytes());
        }

        fn extra_input(path: &str) -> ResponseTemplate {
            let message = format!("{path}: Extra inputs are not permitted");
            ResponseTemplate::new(400).set_body_json(json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": message},
            }))
        }

        fn has_context_management(request: &Request) -> bool {
            sent_json(request).get("context_management").is_some()
        }

        fn has_output_config(request: &Request) -> bool {
            sent_json(request)["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|m| m.get("output_config").is_some()))
        }

        async fn mount_native_ok(copilot: &Copilot) {
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .with_priority(10)
                .mount(&copilot.upstream)
                .await;
        }

        #[tokio::test]
        async fn rejected_top_level_field_is_learned_retried_and_never_sent_again() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .and(has_context_management)
                .respond_with(extra_input("context_management"))
                .with_priority(1)
                .mount(&copilot.upstream)
                .await;
            mount_native_ok(&copilot).await;
            let mut body = anthropic_body(NATIVE_ID, false);
            body["context_management"] = json!({"edits": []});

            let (first, _) = send(&copilot, &client_headers(), &body, RouteKind::Anthropic).await;
            let (second, _) = send(&copilot, &client_headers(), &body, RouteKind::Anthropic).await;

            assert_eq!((first, second), (200, 200));
            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            let carried: Vec<bool> = sent.iter().map(has_context_management).collect();
            assert_eq!(carried, [true, false, false]);
            assert_eq!(
                *copilot.state.unsupported_body_fields(PROFILE),
                [BodyField::TopLevel("context_management".to_string())]
            );
        }

        fn asks_for_compression(request: &Request) -> bool {
            header(&request.headers, "accept-encoding").is_some_and(|value| value != "identity")
        }

        #[tokio::test]
        async fn rejection_is_read_when_the_caller_accepts_gzip_and_it_arrives_chunked() {
            let copilot = copilot().await;
            // Copilot compresses whenever the request allows it; the hub cannot decode that.
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .and(has_context_management)
                .and(asks_for_compression)
                .respond_with(
                    extra_input("context_management").insert_header("content-encoding", "gzip"),
                )
                .with_priority(1)
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .and(has_context_management)
                .respond_with(
                    extra_input("context_management").insert_header("transfer-encoding", "chunked"),
                )
                .with_priority(2)
                .mount(&copilot.upstream)
                .await;
            mount_native_ok(&copilot).await;
            let mut headers = client_headers();
            headers.insert("accept-encoding", HeaderValue::from_static("gzip, br"));
            let mut body = anthropic_body(NATIVE_ID, false);
            body["context_management"] = json!({"edits": []});

            let (status, _) = send(&copilot, &headers, &body, RouteKind::Anthropic).await;

            assert_eq!(status, 200);
            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            let carried: Vec<bool> = sent.iter().map(has_context_management).collect();
            assert_eq!(carried, [true, false]);
            assert_eq!(
                header(&sent[0].headers, "accept-encoding"),
                Some("identity")
            );
            assert_eq!(
                *copilot.state.unsupported_body_fields(PROFILE),
                [BodyField::TopLevel("context_management".to_string())]
            );
        }

        #[tokio::test]
        async fn rejected_message_field_is_stripped_from_every_message() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .and(has_output_config)
                .respond_with(extra_input("messages.1.output_config"))
                .with_priority(1)
                .mount(&copilot.upstream)
                .await;
            mount_native_ok(&copilot).await;
            let mut body = anthropic_body(NATIVE_ID, false);
            body["messages"] = json!([
                {"role": "user", "content": "hi", "output_config": {}},
                {"role": "assistant", "content": "yo", "output_config": {}},
            ]);

            let (status, _) = send(&copilot, &client_headers(), &body, RouteKind::Anthropic).await;

            assert_eq!(status, 200);
            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            assert_eq!(sent.len(), 2);
            assert!(!has_output_config(&sent[1]), "{}", sent_json(&sent[1]));
            assert_eq!(sent_json(&sent[1])["messages"].as_array().unwrap().len(), 2);
        }

        #[tokio::test]
        async fn unrelated_native_400_is_neither_retried_nor_learned() {
            let copilot = copilot().await;
            let error = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: Field required"}}"#;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(
                    ResponseTemplate::new(400).set_body_raw(error.as_bytes(), "application/json"),
                )
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 400);
            assert_eq!(bytes, error.as_bytes());
            assert_eq!(requests_to(&copilot, NATIVE_MESSAGES_PATH).await.len(), 1);
            assert!(copilot.state.unsupported_body_fields(PROFILE).is_empty());
        }

        fn with_beta(mut headers: HeaderMap) -> HeaderMap {
            headers.insert(
                HEADER_ANTHROPIC_BETA,
                HeaderValue::from_static("context-management-2025-06-27"),
            );
            headers
        }

        #[tokio::test]
        async fn anthropic_beta_reaches_native_attempts_only() {
            let copilot = copilot().await;
            mount_native_ok(&copilot).await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;
            let headers = with_beta(client_headers());

            send(
                &copilot,
                &headers,
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;
            send(
                &copilot,
                &headers,
                &anthropic_body(TRANSLATED_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            let native = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            assert_eq!(
                native[0].headers.get(HEADER_ANTHROPIC_BETA).unwrap(),
                "context-management-2025-06-27"
            );
            let translated = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert!(translated[0].headers.get(HEADER_ANTHROPIC_BETA).is_none());
        }

        #[tokio::test]
        async fn anthropic_beta_reaches_native_count_tokens() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_COUNT_TOKENS_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 3})))
                .mount(&copilot.upstream)
                .await;

            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                NATIVE_COUNT_TOKENS_PATH,
                &with_beta(client_headers()),
                Bytes::from(anthropic_body(NATIVE_ID, false).to_string()),
                RouteKind::OpenAi,
            )
            .await
            .unwrap();

            assert_eq!(response.status(), 200);
            let sent = requests_to(&copilot, NATIVE_COUNT_TOKENS_PATH).await;
            assert_eq!(
                sent[0].headers.get(HEADER_ANTHROPIC_BETA).unwrap(),
                "context-management-2025-06-27"
            );
        }

        #[tokio::test]
        async fn claude_model_without_native_messages_is_translated_to_chat_completions() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(TRANSLATED_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["type"], "message", "{reply}");
            let sent = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert_eq!(sent_json(&sent[0])["model"], TRANSLATED_UPSTREAM_ID);
            assert!(requests_to(&copilot, NATIVE_MESSAGES_PATH).await.is_empty());
        }

        #[tokio::test]
        async fn openai_route_sends_the_dashed_id_dotted_and_unknown_ids_unchanged() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;
            let chat = |model: &str| {
                json!({
                    "model": format!("{PROFILE}/{model}"),
                    "messages": [{"role": "user", "content": "hi"}],
                })
            };

            send(
                &copilot,
                &client_headers(),
                &chat(NATIVE_ID),
                RouteKind::OpenAi,
            )
            .await;
            send(
                &copilot,
                &client_headers(),
                &chat(NATIVE_UPSTREAM_ID),
                RouteKind::OpenAi,
            )
            .await;

            let models: Vec<Value> = requests_to(&copilot, UPSTREAM_CHAT_PATH)
                .await
                .iter()
                .map(|request| sent_json(request)["model"].clone())
                .collect();
            assert_eq!(
                models,
                vec![json!(NATIVE_UPSTREAM_ID), json!(NATIVE_UPSTREAM_ID)]
            );
        }

        fn chat_body(model: &str, stream: bool) -> Value {
            json!({
                "model": format!("{PROFILE}/{model}"),
                "stream": stream,
                "messages": [{"role": "user", "content": "hi"}],
            })
        }

        fn anthropic_shaped_reply() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "model": NATIVE_UPSTREAM_ID,
                "content": [{"type": "text", "text": "native"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1},
            }))
        }

        #[tokio::test]
        async fn openai_route_repairs_an_anthropic_shaped_claude_reply() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(anthropic_shaped_reply())
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &chat_body(NATIVE_ID, false),
                RouteKind::OpenAi,
            )
            .await;

            assert_eq!(status, 200);
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["object"], "chat.completion", "{reply}");
            assert_eq!(reply["choices"][0]["message"]["content"], "native");
            assert_eq!(reply["choices"][0]["finish_reason"], "stop");
            assert_eq!(reply["usage"]["prompt_tokens"], 3);
            let sent = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert_eq!(
                header(&sent[0].headers, "accept-encoding"),
                Some("identity")
            );
        }

        #[tokio::test]
        async fn openai_route_repairs_an_anthropic_shaped_claude_stream() {
            let copilot = copilot().await;
            let events = [
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-4.5","usage":{"input_tokens":3}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"native"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ];
            let sse = events
                .map(|data| format!("event: x\ndata: {data}\n\n"))
                .concat();
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &chat_body(NATIVE_ID, true),
                RouteKind::OpenAi,
            )
            .await;

            assert_eq!(status, 200);
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let chunks: Vec<&str> = text
                .split("\n\n")
                .filter_map(|frame| frame.strip_prefix("data: "))
                .collect();
            assert_eq!(chunks.last(), Some(&"[DONE]"), "{text}");
            let content: Value = serde_json::from_str(chunks[1]).unwrap();
            assert_eq!(content["object"], "chat.completion.chunk");
            assert_eq!(content["choices"][0]["delta"]["content"], "native");
            let finish: Value = serde_json::from_str(chunks[2]).unwrap();
            assert_eq!(finish["choices"][0]["finish_reason"], "stop");
        }

        #[tokio::test]
        async fn anthropic_route_reads_an_anthropic_shaped_translated_reply() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(anthropic_shaped_reply())
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(TRANSLATED_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["content"][0]["text"], "native", "{reply}");
        }

        #[tokio::test]
        async fn non_claude_reply_is_never_repaired() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(anthropic_shaped_reply())
                .mount(&copilot.upstream)
                .await;

            let (_, bytes) = send(
                &copilot,
                &client_headers(),
                &chat_body(PLAIN_ID, false),
                RouteKind::OpenAi,
            )
            .await;

            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["type"], "message", "{reply}");
            assert!(reply.get("choices").is_none());
        }

        async fn send_responses(copilot: &Copilot, stream: bool) -> String {
            let body = json!({
                "model": format!("{PROFILE}/{PLAIN_ID}"),
                "stream": stream,
                "input": "hi",
            });
            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                "/v1/responses",
                &client_headers(),
                Bytes::from(body.to_string()),
                RouteKind::OpenAi,
            )
            .await
            .unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        }

        fn responses_stream_with_drifting_ids() -> String {
            [
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_a", "type": "reasoning"}}),
                json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_a", "type": "message"}}),
                json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_b", "delta": "hi"}),
                json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_c", "delta": "!"}),
                json!({"type": "response.output_item.done", "output_index": 1, "item": {"id": "msg_d", "type": "message"}}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "rs_b", "type": "reasoning"}}),
            ]
            .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
            .concat()
        }

        #[tokio::test]
        async fn copilot_responses_stream_keeps_one_item_id_per_output_index() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(responses_stream_with_drifting_ids(), "text/event-stream"),
                )
                .mount(&copilot.upstream)
                .await;

            let text = send_responses(&copilot, true).await;

            let events: Vec<Value> = text
                .split("\n\n")
                .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("data: ")))
                .map(|data| serde_json::from_str(data).unwrap())
                .collect();
            let ids: Vec<&str> = events
                .iter()
                .map(|event| {
                    event["item_id"]
                        .as_str()
                        .or_else(|| event["item"]["id"].as_str())
                        .unwrap()
                })
                .collect();
            assert_eq!(
                ids,
                ["rs_a", "msg_a", "msg_a", "msg_a", "msg_a", "rs_a"],
                "{text}"
            );
            assert_eq!(events[3]["delta"], "!");
            let sent = requests_to(&copilot, "/responses").await;
            assert_eq!(
                header(&sent[0].headers, "accept-encoding"),
                Some("identity")
            );
        }

        #[tokio::test]
        async fn non_streaming_copilot_responses_reply_is_left_alone() {
            let copilot = copilot().await;
            let reply = json!({"id": "resp_1", "output": [{"id": "msg_a", "type": "message"}]});
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&reply))
                .mount(&copilot.upstream)
                .await;

            let text = send_responses(&copilot, false).await;

            assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), reply);
            let sent = requests_to(&copilot, "/responses").await;
            assert_ne!(
                header(&sent[0].headers, "accept-encoding"),
                Some("identity")
            );
        }

        #[tokio::test]
        async fn native_429_falls_back_to_a_translated_attempt() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                    "type": "error",
                    "error": {"type": "rate_limit_error", "message": "slow down"},
                })))
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;
            let mut headers = client_headers();
            headers.insert(
                HEADER_FALLBACKS,
                HeaderValue::from_str(&format!("{PROFILE}/{PLAIN_ID}")).unwrap(),
            );

            let (status, bytes) = send(
                &copilot,
                &headers,
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(reply["type"], "message", "{reply}");
            assert_eq!(reply["content"][0]["text"], "translated", "{reply}");
            assert_eq!(requests_to(&copilot, NATIVE_MESSAGES_PATH).await.len(), 1);
            let chat = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert_eq!(sent_json(&chat[0])["model"], PLAIN_ID);
        }

        const AUTO_SESSION: &str = "auto-session";
        const RESPONSES_ONLY_ID: &str = "gpt-5.5";
        const CHAT_ONLY_ID: &str = "gemini-3-pro";
        const UPSTREAM_RESPONSES_PATH: &str = "/responses";

        /// Lists the catalog Auto routes over and answers `/auto` with `selected`.
        async fn mount_auto(copilot: &Copilot, selected: &str) {
            mount_auto_with_reason(copilot, selected, "test pick").await;
        }

        async fn mount_auto_with_reason(copilot: &Copilot, selected: &str, reason: &str) {
            copilot.state.set_copilot_catalog(
                PROFILE,
                vec![
                    json!({"id": NATIVE_UPSTREAM_ID, "supported_endpoints": ["/chat/completions", "/v1/messages"]}),
                    json!({"id": RESPONSES_ONLY_ID, "supported_endpoints": ["/responses"]}),
                    json!({"id": CHAT_ONLY_ID, "supported_endpoints": ["/chat/completions"]}),
                ],
            );
            Mock::given(method("POST"))
                .and(path("/auto"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "session_token": AUTO_SESSION,
                    "expires_at": u64::MAX / 2,
                    "selected_model": {"id": selected},
                    "selection_reason": reason,
                })))
                .expect(1)
                .mount(&copilot.upstream)
                .await;
        }

        fn session_tokens(request: &Request) -> Vec<String> {
            request
                .headers
                .get_all(HEADER_COPILOT_SESSION_TOKEN)
                .iter()
                .map(|value| value.to_str().unwrap().to_string())
                .collect()
        }

        #[tokio::test]
        async fn auto_on_v1_messages_sends_a_claude_model_natively_with_the_session_token() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_1", "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": "native"}],
                    "usage": {"input_tokens": 3, "output_tokens": 1},
                })))
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let sent = requests_to(&copilot, NATIVE_MESSAGES_PATH).await;
            assert_eq!(sent_json(&sent[0])["model"], NATIVE_UPSTREAM_ID);
            assert_eq!(session_tokens(&sent[0]), [AUTO_SESSION]);
        }

        #[tokio::test]
        async fn auto_on_v1_responses_sends_the_selected_model_with_the_session_token() {
            let copilot = copilot().await;
            mount_auto(&copilot, RESPONSES_ONLY_ID).await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "resp_1"})))
                .mount(&copilot.upstream)
                .await;
            let body = json!({
                "model": format!("{PROFILE}/{COPILOT_AUTO_MODEL}"),
                "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            });

            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                "/v1/responses",
                &client_headers(),
                Bytes::from(body.to_string()),
                RouteKind::OpenAi,
            )
            .await
            .unwrap();

            assert_eq!(response.status(), StatusCode::OK);
            let sent = requests_to(&copilot, UPSTREAM_RESPONSES_PATH).await;
            assert_eq!(sent_json(&sent[0])["model"], RESPONSES_ONLY_ID);
            assert_eq!(session_tokens(&sent[0]), [AUTO_SESSION]);
        }

        #[tokio::test]
        async fn non_auto_request_never_calls_auto_and_sends_no_session_token() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path("/auto"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;
            let body = json!({
                "model": format!("{PROFILE}/{PLAIN_ID}"),
                "messages": [{"role": "user", "content": "hi"}],
            });

            let headers =
                send_for_headers(&copilot, &client_headers(), &body, RouteKind::OpenAi).await;

            let sent = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert_eq!(sent_json(&sent[0])["model"], PLAIN_ID);
            assert!(session_tokens(&sent[0]).is_empty());
            assert_eq!(
                header(&headers, HEADER_SERVED_MODEL),
                Some(format!("{PROFILE}/{PLAIN_ID}").as_str())
            );
            assert_eq!(header(&headers, HEADER_AUTO_PICK), None);
            assert_eq!(header(&headers, HEADER_AUTO_REASON), None);
            assert!(
                copilot
                    .state
                    .copilot_auto()
                    .decisions()
                    .recent(None, 10)
                    .is_empty()
            );
        }

        async fn mount_native_reply(copilot: &Copilot) {
            Mock::given(method("POST"))
                .and(path(NATIVE_MESSAGES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_1", "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": "native"}],
                    "usage": {"input_tokens": 3, "output_tokens": 1},
                })))
                .mount(&copilot.upstream)
                .await;
        }

        fn with_session(session: &str) -> HeaderMap {
            let mut headers = client_headers();
            headers.insert(
                HEADER_CLAUDE_CODE_SESSION_ID,
                HeaderValue::from_str(session).unwrap(),
            );
            headers
        }

        #[tokio::test]
        async fn auto_response_names_the_swapped_model_and_auto_pick_and_records_the_session() {
            let copilot = copilot().await;
            mount_auto(&copilot, CHAT_ONLY_ID).await;
            mount_native_reply(&copilot).await;
            let session = "0f3c-session_1.a";

            let headers = send_for_headers(
                &copilot,
                &with_session(session),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            let served = format!("{PROFILE}/{NATIVE_ID}");
            let auto_pick = format!("{PROFILE}/{CHAT_ONLY_ID}");
            let reason = format!("Auto pick {CHAT_ONLY_ID} does not serve /v1/messages");
            assert_eq!(header(&headers, HEADER_SERVED_MODEL), Some(served.as_str()));
            assert_eq!(header(&headers, HEADER_AUTO_PICK), Some(auto_pick.as_str()));
            assert_eq!(header(&headers, HEADER_AUTO_REASON), Some(reason.as_str()));
            let decisions = copilot
                .state
                .copilot_auto()
                .decisions()
                .recent(Some(session), 1);
            assert_eq!(decisions.len(), 1);
            let decision = &decisions[0];
            assert_eq!(decision.session.as_deref(), Some(session));
            assert_eq!(decision.profile, PROFILE);
            assert_eq!(decision.auto_pick, auto_pick);
            assert_eq!(decision.model, served);
            assert_eq!(decision.swap_reason.as_deref(), Some(reason.as_str()));
            assert_eq!(decision.selection_reason.as_deref(), Some("test pick"));
            assert_eq!(decision.turn, AutoTurn::Routed);
        }

        #[tokio::test]
        async fn invalid_session_id_is_recorded_as_null() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            mount_native_reply(&copilot).await;
            let too_long = "a".repeat(CLAUDE_CODE_SESSION_ID_MAX_LEN + 1);

            send_for_headers(
                &copilot,
                &with_session(&too_long),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            let decisions = copilot.state.copilot_auto().decisions().recent(None, 10);
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0].session, None);
            assert!(
                copilot
                    .state
                    .copilot_auto()
                    .decisions()
                    .recent(Some(&too_long), 1)
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn auto_on_v1_messages_bridges_a_responses_only_pick_without_a_swap() {
            let copilot = copilot().await;
            mount_auto(&copilot, RESPONSES_ONLY_ID).await;
            mount_responses_reply(&copilot, &responses_text_reply("bridged")).await;

            let headers = send_for_headers(
                &copilot,
                &client_headers(),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            let served = format!("{PROFILE}/{RESPONSES_ONLY_ID}");
            assert_eq!(header(&headers, HEADER_SERVED_MODEL), Some(served.as_str()));
            assert_eq!(header(&headers, HEADER_AUTO_REASON), Some("test pick"));
            let sent = requests_to(&copilot, UPSTREAM_RESPONSES_PATH).await;
            assert_eq!(sent_json(&sent[0])["model"], RESPONSES_ONLY_ID);
            assert_eq!(session_tokens(&sent[0]), [AUTO_SESSION]);
            assert!(requests_to(&copilot, NATIVE_MESSAGES_PATH).await.is_empty());
        }

        fn list_responses_only_model(copilot: &Copilot) {
            copilot.state.set_copilot_catalog(
                PROFILE,
                vec![json!({"id": RESPONSES_ONLY_ID, "supported_endpoints": ["/responses"]})],
            );
        }

        async fn mount_responses_reply(copilot: &Copilot, reply: &Value) {
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .respond_with(ResponseTemplate::new(200).set_body_json(reply))
                .mount(&copilot.upstream)
                .await;
        }

        fn responses_text_reply(text: &str) -> Value {
            json!({
                "id": "resp_1",
                "status": "completed",
                "output": [{"type": "message", "id": "m_1", "role": "assistant",
                            "content": [{"type": "output_text", "text": text}]}],
                "usage": {"input_tokens": 9, "output_tokens": 2},
            })
        }

        fn responses_tool_reply() -> Value {
            json!({
                "id": "resp_tool",
                "status": "completed",
                "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_1",
                            "name": "weather", "arguments": "{\"city\":\"Oslo\"}"}],
                "usage": {"input_tokens": 12, "output_tokens": 5},
            })
        }

        async fn send_anthropic(copilot: &Copilot, body: &Value) -> Value {
            let (status, bytes) =
                send(copilot, &client_headers(), body, RouteKind::Anthropic).await;
            assert_eq!(status, 200);
            serde_json::from_slice(&bytes).unwrap()
        }

        #[tokio::test]
        async fn responses_only_model_on_v1_messages_is_bridged_to_responses() {
            let copilot = copilot().await;
            list_responses_only_model(&copilot);
            mount_responses_reply(&copilot, &responses_text_reply("bridged")).await;
            let mut body = anthropic_body(RESPONSES_ONLY_ID, false);
            body["system"] = json!("be terse");

            let reply = send_anthropic(&copilot, &body).await;

            assert_eq!(
                reply,
                json!({
                    "id": "msg_resp_1",
                    "type": "message",
                    "role": "assistant",
                    "model": format!("{PROFILE}/{RESPONSES_ONLY_ID}"),
                    "content": [{"type": "text", "text": "bridged"}],
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {"input_tokens": 9, "output_tokens": 2},
                })
            );
            let sent = requests_to(&copilot, UPSTREAM_RESPONSES_PATH).await;
            assert_eq!(
                sent_json(&sent[0]),
                json!({
                    "model": RESPONSES_ONLY_ID,
                    "instructions": "be terse",
                    "input": [{"type": "message", "role": "user",
                               "content": [{"type": "input_text", "text": "hi"}]}],
                    "max_output_tokens": 64,
                    "stream": false,
                    "reasoning": {"effort": "low"},
                    "store": false,
                })
            );
            assert_eq!(
                header(&sent[0].headers, "accept-encoding"),
                Some("identity")
            );
            assert!(requests_to(&copilot, UPSTREAM_CHAT_PATH).await.is_empty());
        }

        #[tokio::test]
        async fn bridged_stream_pins_item_ids_and_streams_anthropic_events() {
            let copilot = copilot().await;
            list_responses_only_model(&copilot);
            let upstream = [
                json!({"type": "response.created", "response": {"id": "resp_s"}}),
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "msg_a", "type": "message"}}),
                json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "msg_b", "delta": "hi"}),
                json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "msg_c", "delta": "!"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"id": "msg_d", "type": "message"}}),
                json!({"type": "response.completed", "response": {"status": "completed",
                       "usage": {"input_tokens": 4, "output_tokens": 2}}}),
            ]
            .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
            .concat();
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(upstream, "text/event-stream"),
                )
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(RESPONSES_ONLY_ID, true),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let events: Vec<(&str, Value)> = text
                .split("\n\n")
                .filter(|frame| frame.starts_with("event: "))
                .map(|frame| {
                    let (name, data) = frame.split_once('\n').unwrap();
                    let data = data.strip_prefix("data: ").unwrap();
                    (
                        name.trim_start_matches("event: "),
                        serde_json::from_str(data).unwrap(),
                    )
                })
                .collect();
            let names: Vec<&str> = events.iter().map(|(name, _)| *name).collect();
            assert_eq!(
                names,
                [
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ],
                "{text}"
            );
            assert_eq!(
                events[2].1["delta"],
                json!({"type": "text_delta", "text": "hi"})
            );
            assert_eq!(
                events[5].1["usage"],
                json!({"input_tokens": 4, "output_tokens": 2})
            );
            let sent = requests_to(&copilot, UPSTREAM_RESPONSES_PATH).await;
            assert_eq!(sent_json(&sent[0])["stream"], true);
        }

        #[tokio::test]
        async fn bridged_tool_call_round_trips_through_tool_result() {
            let copilot = copilot().await;
            list_responses_only_model(&copilot);
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .and(|request: &Request| {
                    sent_json(request)["input"].as_array().is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["type"] == "function_call_output")
                    })
                })
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(responses_text_reply("sunny in Oslo")),
                )
                .with_priority(1)
                .mount(&copilot.upstream)
                .await;
            mount_responses_reply(&copilot, &responses_tool_reply()).await;
            let tools = json!([{"name": "weather", "input_schema": {"type": "object",
                "properties": {"city": {"type": "string"}}}}]);
            let mut first_turn = anthropic_body(RESPONSES_ONLY_ID, false);
            first_turn["tools"] = tools.clone();

            let tool_call = send_anthropic(&copilot, &first_turn).await;

            assert_eq!(tool_call["stop_reason"], "tool_use");
            let tool_use = tool_call["content"][0].clone();
            assert_eq!(
                tool_use,
                json!({"type": "tool_use", "id": "call_1", "name": "weather", "input": {"city": "Oslo"}})
            );

            let mut second_turn = first_turn.clone();
            second_turn["messages"] = json!([
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [tool_use]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "sunny"},
                ]},
            ]);
            let answer = send_anthropic(&copilot, &second_turn).await;

            assert_eq!(
                answer["content"],
                json!([{"type": "text", "text": "sunny in Oslo"}])
            );
            assert_eq!(answer["stop_reason"], "end_turn");
            let sent = requests_to(&copilot, UPSTREAM_RESPONSES_PATH).await;
            assert_eq!(sent.len(), 2);
            assert_eq!(
                sent_json(&sent[1])["input"],
                json!([
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "weather",
                     "arguments": "{\"city\":\"Oslo\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
                ])
            );
            assert_eq!(sent_json(&sent[1])["tools"][0]["name"], "weather");
        }

        #[tokio::test]
        async fn bridged_error_reply_is_forwarded_unmodified() {
            let copilot = copilot().await;
            list_responses_only_model(&copilot);
            let error = json!({"error": {"message": "bad input", "code": "invalid"}});
            Mock::given(method("POST"))
                .and(path(UPSTREAM_RESPONSES_PATH))
                .respond_with(ResponseTemplate::new(400).set_body_json(&error))
                .mount(&copilot.upstream)
                .await;

            let (status, bytes) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(RESPONSES_ONLY_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 400);
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), error);
        }

        #[test]
        fn session_id_outside_the_allowed_charset_is_dropped() {
            let session = |value: &str| claude_code_session_id(&with_session(value));

            assert_eq!(session("abc.DEF_1-2").as_deref(), Some("abc.DEF_1-2"));
            assert_eq!(session(""), None);
            assert_eq!(session("has space"), None);
            assert_eq!(session("semi;colon"), None);
            assert_eq!(claude_code_session_id(&client_headers()), None);
        }

        #[tokio::test]
        async fn non_ascii_selection_reason_is_sanitised_and_truncated_in_the_header() {
            let copilot = copilot().await;
            let reason = format!("medium complexity \u{2014} {}", "x".repeat(400));
            mount_auto_with_reason(&copilot, NATIVE_UPSTREAM_ID, &reason).await;
            mount_native_reply(&copilot).await;

            let headers = send_for_headers(
                &copilot,
                &client_headers(),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            let sent = header(&headers, HEADER_AUTO_REASON).unwrap();
            assert_eq!(sent.len(), AUTO_REASON_HEADER_MAX_CHARS);
            assert!(sent.starts_with("medium complexity ? xxx"), "{sent}");
            let auto_pick = format!("{PROFILE}/{NATIVE_ID}");
            assert_eq!(header(&headers, HEADER_AUTO_PICK), Some(auto_pick.as_str()));
            assert_eq!(
                header(&headers, HEADER_SERVED_MODEL),
                Some(auto_pick.as_str())
            );
        }
    }

    #[tokio::test]
    async fn a_ping_is_never_fed_through_the_transform() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
        let delta = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let mut state = state_for(
            slow_stream(vec![(SILENCE_MS, delta), (0, "data: [DONE]\n\n")]),
            Some(KEEPALIVE),
            Some(sender),
        );
        state.transform = Some(BodyTransform::anthropic_stream(
            "p/m".to_string(),
            crate::services::tool_names::NameMap::default(),
        ));
        let chunks = drain(state).await;

        assert!(ping_count(&chunks) >= 2, "{chunks:?}");
        // The translator only ever saw the upstream frame, so it produced a
        // complete Anthropic message and no ping leaked into an event payload.
        let translated: String = chunks
            .iter()
            .filter(|c| *c != SSE_KEEPALIVE_FRAME)
            .cloned()
            .collect();
        assert!(translated.contains("event: message_start"), "{translated}");
        assert!(translated.contains("event: message_stop"), "{translated}");
        assert!(!translated.contains("keepalive"), "{translated}");
        assert!(receiver.await.is_ok());
    }
}

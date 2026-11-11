use std::borrow::Cow;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;

use crate::configs::{HubConfig, ProfileConfig};
use crate::consts::{
    ANTHROPIC_UPSTREAM_PATH, AUTO_REASON_HEADER_MAX_CHARS, ERROR_REASON_MAX_CHARS,
    ERROR_STATUS_MIN, HEADER_ATTEMPTS, HEADER_AUTO_PICK, HEADER_AUTO_REASON, HEADER_SERVED_MODEL,
    HEADER_TRANSFORMS, KEEP_WARM_CLIENT, MAX_MULTIPART_BYTES, MAX_TRANSFORM_BUFFER_BYTES,
    PROMPT_REFUSAL_MARGIN_PERCENT, SSE_KEEPALIVE_FRAME, USAGE_SCRAPE_TAIL_BYTES,
};
use crate::dependencies::state::AppState;
use crate::routes::models::{COPILOT_AUTO_MODEL, advertised_bare_id};
use crate::schemas::affinity_key::AffinityKey;
use crate::schemas::app_error::AppError;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::schemas::model_id::ModelId;
use crate::schemas::prompt_shape::PromptShape;
use crate::schemas::reply_protocol::ReplyProtocol;
use crate::schemas::review_evidence::ReviewEvidence;
use crate::schemas::review_request::ReviewRequest;
use crate::schemas::route_reason::RouteReason;
use crate::schemas::scope_kind::ScopeKind;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::schemas::smart_auto_decision::SmartAutoDecision;
use crate::schemas::smart_auto_headers::SmartAutoHeaders;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::stage_error::StageError;
use crate::schemas::stage_reply::StageReply;
use crate::schemas::turn_signals::TurnSignals;
use crate::schemas::upstream_model::UpstreamModel;
use crate::schemas::work_role::WorkRole;
use crate::services::anthropic;
use crate::services::anthropic::native_reply::NativeReplyRepair;
use crate::services::anthropic::responses_stream::ResponsesStream;
use crate::services::body_transform::BodyTransform;
use crate::services::buffered_reply_stream::BufferedReplyStream;
use crate::services::copilot_auto::{
    AutoInput, AutoTier, AutoUpstream, CHAT_ENDPOINT, MESSAGES_ENDPOINT, RESPONSES_ENDPOINT,
    bare_path, estimated_prompt_tokens,
};
use crate::services::copilot_auto_decisions::{AutoDecision, TieredDecision};
use crate::services::copilot_body_fields::BodyField;
use crate::services::copilot_headers::{HEADER_X_INITIATOR, INITIATOR_AGENT};
use crate::services::fallback::{
    RetryPolicy, fallback_chain, per_attempt_timeout_ms, retain_eligible,
};
use crate::services::harness::client_signature;
use crate::services::keep_warm::{KeepWarmLease, PingPlan};
use crate::services::multipart_model;
use crate::services::request_images::request_has_image;
use crate::services::responses_item_ids::ResponsesItemIds;
use crate::services::review_graph::ReviewGraph;
use crate::services::review_orchestrator::ReviewOrchestrator;
use crate::services::smart_auto_effort::SmartAutoEffort;
use crate::services::smart_auto_pool::SmartAutoPool;
use crate::services::smart_auto_router::SmartAutoRouter;
use crate::services::stage_client::StageClient;
use crate::services::stats::{RequestOutcome, ScrapedUsage, scrape_stream_usage, scrape_usage};
use crate::services::store::now_ms;
use crate::services::tool_names::NameMap;
use crate::services::transforms::{self, RouteKind, TransformPlan};
use crate::services::upstream_auth;
use crate::utils::headers::{filter_request_headers, filter_response_headers};

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
/// First bytes of each response kept for usage that only the opening event reports.
const USAGE_SCRAPE_HEAD_BYTES: usize = 16 * 1024;
/// Bytes kept of one dispatch's prompt and of its answer for the Smart Auto trace. Each is cut at this length.
const CAPTURE_TEXT_BYTES: usize = 256 * 1024;
/// The client-facing chat route that review stages call on a concrete model.
const STAGE_CHAT_PATH: &str = "/v1/chat/completions";
/// The text an upstream 500 carries when the model refuses image input.
const IMAGE_INPUT_REJECTION: &str = "image input is not supported";
static NO_BODY: Value = Value::Null;
/// The status codes that count as a success for Smart Auto affinity.
const SUCCESS_STATUS_RANGE: std::ops::Range<u16> = 200..300;

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
#[allow(clippy::too_many_lines)] // the attempt state machine reads top to bottom; splitting it would thread the same loop state through helpers
pub(crate) async fn attempt_loop(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Bytes,
    route: RouteKind,
) -> Result<Response, AppError> {
    let config = state.config();
    let (bodies, requested_id) =
        AttemptBodies::prepare(&config, path_and_query, headers, &body, route)?;

    let fallbacks = fallback_chain(headers, &config.default_fallbacks);
    let (primary_id, fallbacks, smart) =
        match resolve_hub(state, &config, headers, &bodies, &requested_id, fallbacks)? {
            HubResolution::Concrete {
                primary,
                fallbacks,
                plan,
            } => (primary, fallbacks, Some(plan)),
            HubResolution::NotHub(fallbacks) => (requested_id, fallbacks, None),
        };

    let mut chain: Vec<ModelId> = vec![primary_id];
    for fallback in fallbacks {
        if let Some(id) = ModelId::parse(&fallback) {
            chain.push(id);
        }
    }
    let needs = RequestNeeds {
        path: path_and_query,
        input: bodies.auto_input(),
        prompt_tokens: estimated_prompt_tokens(bodies.original.len()),
    };
    let mut refusals: Vec<String> = Vec::new();
    let chain: Vec<ModelId> = chain
        .into_iter()
        .filter_map(|model| match refusal(state, &config, &model, &needs) {
            None => Some(model),
            Some(reason) => {
                refusals.push(reason);
                None
            }
        })
        .collect();
    if chain.is_empty() {
        return Err(AppError::BadRequest(refusals.join("; ")));
    }

    let client_path = bare_path(path_and_query);
    let client = client_signature(headers);
    let worker = worker_key(state, headers);
    let retry_policy = RetryPolicy::from_headers(headers);
    // The raw client body, kept only for the Anthropic route's prompt shape row.
    let shape_body = (bodies.route == RouteKind::Anthropic).then(|| body.clone());
    let attempt_timeout = per_attempt_timeout_ms(headers).map(Duration::from_millis);
    let client_body = bodies
        .native
        .as_ref()
        .map(|(body, _)| body)
        .or(bodies.json_body.as_ref());
    let client_streams = client_body
        .and_then(|body| body.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut attempts_trail: Vec<String> = Vec::new();
    let last_index = chain.len() - 1;
    // The selection template of the routed request. Every dispatch in the chain shares it.
    let mut routed = smart.as_ref().map(|plan| plan.template.clone());

    for (index, requested) in chain.iter().enumerate() {
        let auto = resolve_auto(state, &bodies, requested).await;
        if let Some(pick) = &auto {
            routed = Some(pick.template.clone());
        }
        let model_id = auto.as_ref().map_or(requested, |pick| &pick.model_id);
        let routed_by = routed_by_label(auto.is_some(), smart.as_ref().map(|plan| plan.tier));
        let mut attempt = bodies.for_model(state, model_id);
        let effort = match (&smart, &auto) {
            (Some(plan), _) => plan.effort_for(state, model_id),
            (None, Some(pick)) => pick.effort,
            (None, None) => None,
        };
        if let Some(level) = effort {
            let body = std::mem::take(&mut attempt.body);
            attempt.body =
                SmartAutoEffort::apply_to_bytes(bare_path(attempt.path.as_str()), body, level);
        }
        // Selected is recorded before the dispatch, so the current selection shows it while the call is in flight.
        let selection = routed.as_ref().map(|template| {
            let event = selected_event(state, template, model_id, effort);
            state.selection_log().record(event.clone());
            event
        });
        if let Some(event) = &selection {
            save_prompt(state, event, &attempt.body);
        }
        let reviewing = smart
            .as_ref()
            .filter(|plan| plan.review.is_some() && review_applies(&attempt));
        if reviewing.is_some() {
            let body = std::mem::take(&mut attempt.body);
            attempt.body = non_streaming(body);
        }
        let started = Instant::now();
        let session_token = auto.as_ref().and_then(|pick| pick.session_token.as_deref());
        let sender = AttemptSender::for_attempt(
            state,
            &method,
            &attempt,
            model_id,
            attempt_timeout,
            session_token,
        );
        let mut keep_warm = keep_warm_lease(state, &attempt, model_id, worker.as_deref());
        let (result, retried_body) = match sender.send(attempt.body.clone()).await {
            Ok(response) if attempt.native_messages => {
                match retry_without_rejected_fields(&bodies, &sender, response).await {
                    Ok((response, retried)) => (Ok(response), retried),
                    Err(reason) => (Err(reason), None),
                }
            }
            Ok(response) => (Ok(attempt.reply_rewrite.apply(response).await), None),
            other => (other, None),
        };

        match result {
            Ok(response) => {
                let status = response.status().as_u16();
                attempts_trail.push(format!("{}={status}", model_id.qualified()));
                let final_attempt =
                    index == last_index || !retry_policy.should_retry_status(status);
                let succeeded = final_attempt && SUCCESS_STATUS_RANGE.contains(&status);
                let transition = selection.as_ref().map(|event| {
                    let next = if succeeded {
                        SelectionState::Started
                    } else {
                        SelectionState::Rejected
                    };
                    record_transition(state, event, next, Some(status))
                });
                if final_attempt {
                    let commit = smart
                        .as_ref()
                        .and_then(|plan| plan.commit(status, model_id));
                    let stream_lease = keep_warm.take().filter(|_| succeeded).map(|mut lease| {
                        let accepted = retried_body.unwrap_or_else(|| attempt.body.clone());
                        let beta = attempt.headers.get(HEADER_ANTHROPIC_BETA).cloned();
                        lease.record_stream(accepted, model_id.clone(), started, beta);
                        lease
                    });
                    let mut out = match reviewing.filter(|_| succeeded) {
                        Some(plan) => {
                            review_deliver(
                                state,
                                response,
                                model_id,
                                &attempts_trail,
                                started,
                                bodies.route,
                                plan,
                                client_streams,
                                client,
                                worker,
                                commit,
                                transition.filter(|_| succeeded),
                                routed_by.clone(),
                            )
                            .await?
                        }
                        None => {
                            deliver(
                                state,
                                response,
                                model_id,
                                &attempts_trail,
                                started,
                                &attempt,
                                client_path,
                                client,
                                worker,
                                commit,
                                transition.filter(|_| succeeded),
                                shape_body.clone(),
                                routed_by.clone(),
                                stream_lease,
                            )
                            .await
                        }
                    };
                    if let Some(pick) = &auto {
                        insert_header(out.headers_mut(), HEADER_AUTO_PICK, &pick.pick);
                        insert_header(out.headers_mut(), HEADER_AUTO_REASON, &pick.reason);
                        insert_header(out.headers_mut(), HEADER_AUTO_TIER, pick.tier.as_str());
                    }
                    if let Some(plan) = &smart {
                        let headers = out.headers_mut();
                        insert_header(headers, SmartAutoHeaders::TIER, plan.tier.as_str());
                        insert_header(headers, SmartAutoHeaders::ROLE, plan.role.as_str());
                        insert_header(headers, SmartAutoHeaders::REASON, plan.reason.as_str());
                        if let Some(level) = effort {
                            insert_header(headers, SmartAutoHeaders::EFFORT, level.as_str());
                        }
                    }
                    return Ok(out);
                }
                let rejected_model = rejected_limit_model(
                    state,
                    response.status(),
                    response.headers(),
                    model_id,
                    attempt.path.as_str(),
                );
                if rejected_model.is_some() || status == StatusCode::INTERNAL_SERVER_ERROR.as_u16()
                {
                    let (body, _) =
                        read_upstream_capped(response, MAX_TRANSFORM_BUFFER_BYTES).await;
                    learn_image_refusal(state, model_id, status, &body);
                    if let Some(upstream) = &rejected_model {
                        state.learn_rejected_prompt_limit(&model_id.profile, upstream, &body);
                    }
                }
                record_failure(
                    state,
                    model_id,
                    status,
                    started,
                    format!("HTTP {status}; retried on the next fallback"),
                    client.clone(),
                    worker.clone(),
                    routed_by.clone(),
                    LogPaths {
                        client: client_path,
                        upstream: attempt.path.as_str(),
                    },
                );
            }
            Err(reason) => {
                if let Some(event) = &selection {
                    record_transition(state, event, SelectionState::Rejected, None);
                }
                attempts_trail.push(format!("{}={reason}", model_id.qualified()));
                record_failure(
                    state,
                    model_id,
                    0,
                    started,
                    reason,
                    client.clone(),
                    worker.clone(),
                    routed_by.clone(),
                    LogPaths {
                        client: client_path,
                        upstream: attempt.path.as_str(),
                    },
                );
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

/// `routed_by` of a request the client pinned to one concrete model.
const ROUTED_BY_PINNED: &str = "pinned";
/// `routed_by` of a request that Copilot Auto picked.
const ROUTED_BY_COPILOT_AUTO: &str = "copilot-auto";

/// How a request's model was chosen, for its request row: Copilot Auto, a Smart Auto tier, or the client's own model.
fn routed_by_label(copilot_auto: bool, smart_tier: Option<SmartAutoTier>) -> String {
    match (copilot_auto, smart_tier) {
        (true, _) => ROUTED_BY_COPILOT_AUTO.to_owned(),
        (false, Some(tier)) => format!("smart-auto:{}", tier.as_str()),
        (false, None) => ROUTED_BY_PINNED.to_owned(),
    }
}

/// What a requested model routes as: a `hub/*` selector resolved to a concrete model, or any
/// other model with its fallback chain unchanged.
enum HubResolution {
    Concrete {
        primary: ModelId,
        fallbacks: Vec<String>,
        plan: Box<SmartAutoPlan>,
    },
    NotHub(Vec<String>),
}

/// The Smart Auto route of one request, kept until its response is sent.
struct SmartAutoPlan {
    /// The conversation's affinity key, `None` when the request names no anchor.
    key: Option<AffinityKey>,
    tier: SmartAutoTier,
    role: WorkRole,
    plan_backed: bool,
    /// The user turn that the soft-switch budget counts.
    turn: usize,
    /// The effort the caller sent, which every model keeps when it supports it.
    requested: Option<EffortLevel>,
    /// The routed model, as a qualified id.
    model: String,
    /// The effort sent to the routed model.
    effort: Option<EffortLevel>,
    reason: RouteReason,
    /// The selection event each dispatch of this route starts from.
    template: SelectionEvent,
    /// The review of this route's answers; `None` serves a single stream.
    review: Option<ReviewGraph>,
    /// The latest user text, which reviewers read as the task.
    task: Option<String>,
}

/// What a successful routed answer commits to affinity.
#[derive(Clone)]
struct SmartAutoCommit {
    key: AffinityKey,
    model: String,
    role: WorkRole,
    plan_backed: bool,
    turn: usize,
}

impl SmartAutoCommit {
    /// Pins the committed model for its role and plan backing in the affinity table.
    fn apply(&self, state: &AppState) {
        state.smart_auto_affinity().commit(
            &self.key,
            &self.model,
            self.role,
            self.plan_backed,
            self.turn,
            Instant::now(),
        );
    }
}

impl SmartAutoPlan {
    /// The effort for `model_id`: the routed effort on the routed model, else the same objective
    /// snapped to that model's own published levels.
    fn effort_for(&self, state: &AppState, model_id: &ModelId) -> Option<EffortLevel> {
        if model_id.qualified() == self.model {
            return self.effort;
        }
        let snapshot = state.model_descriptors();
        let levels = snapshot
            .for_model(&model_id.profile, &model_id.model)?
            .effort_levels
            .as_slice();
        SmartAutoEffort::select(
            self.tier,
            self.role,
            self.plan_backed,
            self.requested,
            levels,
        )
    }

    /// The commit for an answer with `status` from `served`. Only a 2xx commits, and only once
    /// the body has completed.
    fn commit(&self, status: u16, served: &ModelId) -> Option<SmartAutoCommit> {
        let key = self.key?;
        if !SUCCESS_STATUS_RANGE.contains(&status) {
            return None;
        }
        Some(SmartAutoCommit {
            key,
            model: served.qualified(),
            role: self.role,
            plan_backed: self.plan_backed,
            turn: self.turn,
        })
    }
}

/// Resolves a `hub/*` primary to the pick from the eligible pool, and keeps only pool fallbacks,
/// so deny wins on every attempt. In memory only; any other model costs one string compare.
#[allow(clippy::too_many_lines)] // the hub resolution walks each selector form in one place
fn resolve_hub(
    state: &AppState,
    config: &HubConfig,
    headers: &HeaderMap,
    bodies: &AttemptBodies<'_>,
    requested: &ModelId,
    fallbacks: Vec<String>,
) -> Result<HubResolution, AppError> {
    if requested.profile != HubSelector::NAMESPACE {
        return Ok(HubResolution::NotHub(fallbacks));
    }
    let Some(selector) = HubSelector::parse(&requested.qualified(), &config.smart_auto_presets)?
    else {
        return Ok(HubResolution::NotHub(fallbacks));
    };
    let caller = SmartAutoHeaders::from_headers(headers)?;
    // The tier is chosen by the model id; a header that names another tier is refused, never ignored.
    if let Some(header_tier) = caller.policy.tier.filter(|tier| *tier != selector.tier) {
        return Err(AppError::BadRequest(format!(
            "tier header `{}` conflicts with model `{}`: use the matching model id or remove the header",
            header_tier.as_str(),
            selector.name,
        )));
    }
    let rules = state.smart_auto_rules().current();
    let policy = rules.layer(
        caller.rules_mode.unwrap_or_default(),
        &selector.policy,
        &caller.policy,
    );
    let endpoint = match bodies.route {
        RouteKind::OpenAi => bare_path(bodies.upstream_path),
        RouteKind::Anthropic => MESSAGES_ENDPOINT,
    };
    let (path, body) = bodies.auto_input();
    let signals = TurnSignals::from_body(
        path,
        body,
        bodies.original.len(),
        claude_code_session_id(headers),
    );
    let profiles = concrete_profile_filter(headers);
    let snapshot = state.model_descriptors();
    let mut pool = SmartAutoPool::eligible_for(
        &snapshot,
        &policy,
        endpoint,
        signals.estimated_prompt_tokens,
        signals.has_image,
    );
    if let Some(profiles) = &profiles {
        pool.retain(|descriptor| profiles.allows(&descriptor.qualified_id));
    }
    let affinity_key = AffinityKey::new(&signals, &caller, &policy, endpoint);
    let affinity_scope = AffinityKey::scope(&signals, &caller);
    let selector_name = requested.qualified();
    // Smart Auto ignores the effort the client sends: the hub picks it from the tier and role.
    let requested = None;
    let usage = state.model_usage();
    let anchor = affinity_key
        .as_ref()
        .map(|key| (state.smart_auto_affinity(), key));
    let decision = SmartAutoRouter::route(
        &pool,
        &policy,
        &signals,
        anchor,
        &usage,
        requested,
        Instant::now(),
    )
    .ok_or_else(|| {
        no_eligible_model(
            &selector,
            &policy,
            profiles.as_ref(),
            endpoint,
            signals.has_image,
        )
    })?;
    let primary = ModelId::parse(&decision.model.qualified_id).ok_or_else(|| {
        no_eligible_model(
            &selector,
            &policy,
            profiles.as_ref(),
            endpoint,
            signals.has_image,
        )
    })?;
    let tier = policy.tier.unwrap_or_default();
    state.smart_auto_decisions().record(SmartAutoDecision {
        ts_ms: now_ms(),
        session: signals.session_id.clone(),
        tier: tier.as_str(),
        role: decision.role,
        effort: decision.effort.map(EffortLevel::as_str),
        model: decision.model.qualified_id.clone(),
        reason: decision.reason.as_str(),
    });
    let log = state.selection_log();
    let worker_raw = caller.worker.clone().or_else(|| signals.session_id.clone());
    let worker = worker_raw
        .as_deref()
        .map(|raw| log.scope_key(ScopeKind::Worker, raw));
    let template = SelectionEvent {
        workflow: caller
            .parent
            .as_deref()
            .map(|raw| log.scope_key(ScopeKind::Workflow, raw)),
        turn: worker
            .as_ref()
            .map(|key| log.scope_key(ScopeKind::Turn, &format!("{key}:{}", signals.user_turns))),
        worker,
        selector: Some(selector_name),
        tier: Some(tier.as_str().to_owned()),
        role: Some(decision.role),
        plan_backed: Some(decision.plan_backed),
        affinity_scope,
        pin_miss: decision.pin_miss,
        reason: Some(decision.reason.as_str().to_owned()),
        requested_effort: requested,
        ..SelectionEvent::new(
            SelectionStage::Solver,
            SelectionState::Selected,
            decision.model.qualified_id.clone(),
        )
    };
    let review = ReviewGraph::plan(
        &ReviewRequest {
            tier,
            role: decision.role,
            plan_backed: decision.plan_backed,
            solver: &decision.model.qualified_id,
            policy: &policy,
            snapshot: snapshot.as_ref(),
            endpoint,
            evidence_tokens: signals.estimated_prompt_tokens,
            admits: &|id: &str| profiles.as_ref().is_none_or(|filter| filter.allows(id)),
        },
        &|profile: &str| {
            config
                .profile(profile)
                .is_some_and(|found| found.managed.is_some())
        },
    );
    if review.is_none()
        && is_responses_endpoint(endpoint)
        && matches!(tier, SmartAutoTier::Balance | SmartAutoTier::Intelligence)
    {
        // A Responses caller is streamed unreviewed; history names the skip.
        state.selection_log().record(SelectionEvent {
            reason: Some("review_skipped: responses_route".to_owned()),
            ..template.in_state(SelectionState::Selected)
        });
    }
    let plan = SmartAutoPlan {
        key: affinity_key,
        tier,
        role: decision.role,
        plan_backed: decision.plan_backed,
        turn: signals.user_turns,
        requested,
        model: decision.model.qualified_id.clone(),
        effort: decision.effort,
        reason: decision.reason,
        template,
        review,
        task: signals.last_user_text.clone(),
    };
    Ok(HubResolution::Concrete {
        primary,
        fallbacks: retain_eligible(fallbacks, Some(&pool)),
        plan: Box::new(plan),
    })
}

/// The caller's `x-llm-hub-profiles` filter when it names a concrete profile. The `hub` name
/// only lists the virtual models, so a filter of `hub` alone leaves the pool unrestricted.
fn concrete_profile_filter(headers: &HeaderMap) -> Option<ProfileFilter> {
    let profiles = ProfileFilter::from_request(None, headers);
    profiles
        .to_list()
        .split(',')
        .any(|name| !name.is_empty() && name != HubSelector::NAMESPACE)
        .then_some(profiles)
}

/// The 400 for a `hub/*` request that no model can serve, naming every active restriction.
fn no_eligible_model(
    selector: &HubSelector,
    policy: &SmartAutoPolicy,
    profiles: Option<&ProfileFilter>,
    endpoint: &str,
    has_image: bool,
) -> AppError {
    let mut restrictions = vec![format!("tier {}", selector.tier.as_str())];
    if let Some(profiles) = profiles {
        restrictions.push(format!("profiles {}", profiles.to_list().replace(',', "|")));
    }
    for allow_list in &policy.include {
        let globs: Vec<&str> = allow_list.iter().map(String::as_str).collect();
        restrictions.push(format!("include {}", globs.join("|")));
    }
    if !policy.exclude.is_empty() {
        let globs: Vec<&str> = policy.exclude.iter().map(String::as_str).collect();
        restrictions.push(format!("exclude {}", globs.join("|")));
    }
    for (name, minimum) in [
        ("min context", policy.min_context_tokens),
        ("min input", policy.min_input_tokens),
        ("min output", policy.min_output_tokens),
    ] {
        if let Some(minimum) = minimum {
            restrictions.push(format!("{name} {minimum} tokens"));
        }
    }
    if has_image {
        restrictions.push("image input".to_owned());
    }
    AppError::BadRequest(format!(
        "smart_auto_no_eligible_model: no model serves {namespace}/{name} on {endpoint} under: {}",
        restrictions.join(", "),
        namespace = HubSelector::NAMESPACE,
        name = selector.name,
    ))
}

/// What one request needs from the model that serves it, read once before the chain runs.
struct RequestNeeds<'a> {
    /// The client path, with any query string.
    path: &'a str,
    /// The upstream path and body. Scanned for images only when a model refuses them.
    input: (&'a str, &'a Value),
    /// Roughly one token per four request bytes.
    prompt_tokens: u64,
}

/// Why `model_id` cannot serve the request, or `None` when it may be tried. Reads memory only.
/// A prompt within `PROMPT_REFUSAL_MARGIN_PERCENT` of the limit is tried, because the estimate overstates.
fn refusal(
    state: &AppState,
    config: &HubConfig,
    model_id: &ModelId,
    needs: &RequestNeeds<'_>,
) -> Option<String> {
    let qualified = model_id.qualified();
    let is_copilot = config
        .profile(&model_id.profile)
        .is_some_and(|found| found.copilot.is_some());
    if is_copilot && AutoTier::from_model(&model_id.model).is_some() {
        // Copilot Auto checks the image and the prompt size of the model it picks.
        return (!serves_chat_path(needs.path)).then(|| {
            format!(
                "{qualified} routes chat requests only; pick a model for {}",
                bare_path(needs.path)
            )
        });
    }
    let snapshot = state.model_descriptors();
    let descriptor = snapshot.for_model(&model_id.profile, &model_id.model);
    let upstream = upstream_key(state, model_id);
    let refuses_images = descriptor.and_then(|found| found.vision) == Some(false)
        || state.vision_unsupported(&model_id.profile, &upstream);
    if refuses_images && request_has_image(needs.input.0, needs.input.1) {
        return Some(format!("{qualified}: image input is not supported"));
    }
    let limit = known_prompt_limit(state, model_id, is_copilot, &upstream, descriptor)?;
    (needs.prompt_tokens.saturating_mul(100) > limit.saturating_mul(PROMPT_REFUSAL_MARGIN_PERCENT)).then(|| {
        format!(
            "{qualified}: a prompt of about {} tokens is over the known limit of {limit} tokens",
            needs.prompt_tokens
        )
    })
}

/// The smallest prompt size `model_id` is known to accept: a Copilot model's seeded or learned limit,
/// else the ceiling its descriptor reports, and any limit an upstream rejection named. `None` when none is known.
fn known_prompt_limit(
    state: &AppState,
    model_id: &ModelId,
    is_copilot: bool,
    upstream: &str,
    descriptor: Option<&ModelDescriptor>,
) -> Option<u64> {
    let configured = if is_copilot
        && let Some(entry) = state
            .model_limits()
            .for_profile(&model_id.profile)
            .get(upstream)
    {
        Some(entry.limit)
    } else {
        descriptor.and_then(descriptor_ceiling)
    };
    let rejected = state.rejected_prompt_limit(&model_id.profile, upstream);
    [configured, rejected].into_iter().flatten().min()
}

/// The smallest prompt or runtime capacity a descriptor reports. Conflicting and lower-bound
/// values are not ceilings.
fn descriptor_ceiling(descriptor: &ModelDescriptor) -> Option<u64> {
    [
        descriptor.input.as_ref().or(descriptor.context.as_ref()),
        descriptor.runtime_context.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter(|evidence| !evidence.conflict && !evidence.source.is_lower_bound())
    .map(|evidence| evidence.value)
    .min()
}

/// True for the routes Copilot Auto routes: chat completions, Responses and Anthropic messages.
fn serves_chat_path(path: &str) -> bool {
    let bare = bare_path(path);
    [CHAT_ENDPOINT, RESPONSES_ENDPOINT, MESSAGES_ENDPOINT]
        .into_iter()
        .any(|endpoint| bare.ends_with(endpoint))
}

/// The model id `model_id` is sent as: the catalog's upstream id, else the id the client wrote.
fn upstream_key(state: &AppState, model_id: &ModelId) -> String {
    state
        .upstream_model(&model_id.profile, &model_id.model)
        .map_or_else(|| model_id.model.clone(), |entry| entry.upstream_id)
}

/// Learns that `model_id` refuses image input when an upstream 500 body says so.
fn learn_image_refusal(state: &AppState, model_id: &ModelId, status: u16, body: &[u8]) {
    if status != StatusCode::INTERNAL_SERVER_ERROR.as_u16() {
        return;
    }
    if !String::from_utf8_lossy(body).contains(IMAGE_INPUT_REJECTION) {
        return;
    }
    state.learn_vision_unsupported(&model_id.profile, &upstream_key(state, model_id));
}

/// The request in each form an attempt can send: translated for a chat-completions
/// upstream, or the caller's own Anthropic body for a native `/v1/messages` model.
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
        // Server tools are dropped here: Copilot's native endpoint rejects them with a 400.
        let native = json_body
            .clone()
            .filter(|_| route == RouteKind::Anthropic)
            .map(|mut anthropic_body| {
                anthropic::request::drop_server_tools(&mut anthropic_body);
                (anthropic_body, with_anthropic_version(headers))
            });

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
                path: UpstreamPath::Verbatim(MESSAGES_ENDPOINT),
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
            path: UpstreamPath::Verbatim(RESPONSES_ENDPOINT),
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
            Some((anthropic_body, _)) => (MESSAGES_ENDPOINT, anthropic_body),
            None => (
                UpstreamPath::Strip(self.upstream_path).as_str(),
                self.json_body.as_ref().unwrap_or(&NO_BODY),
            ),
        }
    }
}

/// The Auto tier a `<profile>/auto` or `<profile>/auto-<tier>` request routed on.
const HEADER_AUTO_TIER: &str = "x-llm-hub-auto-tier";

/// The model sent in place of `<profile>/auto` or `<profile>/auto-<tier>`, its
/// Copilot session token, and the values of the Auto response headers.
struct AutoPick {
    model_id: ModelId,
    session_token: Option<String>,
    tier: AutoTier,
    /// Auto's own choice, as a qualified id.
    pick: String,
    /// Header-safe: the swap reason, else Auto's selection reason, else `auto`.
    reason: String,
    /// The effort sent with `model_id`.
    effort: Option<EffortLevel>,
    /// The selection event each dispatch of this pick starts from.
    template: SelectionEvent,
}

/// Resolves a Copilot `<profile>/auto[-<tier>]` through Copilot Auto and logs the decision.
/// Any other model returns `None` before any lookup.
async fn resolve_auto(
    state: &AppState,
    bodies: &AttemptBodies<'_>,
    model_id: &ModelId,
) -> Option<AutoPick> {
    let tier = AutoTier::from_model(&model_id.model)?;
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
    let upstream = AutoUpstream {
        client: &client,
        profile: &profile.name,
        bearer: &bearer,
        api_base: &api_base,
    };
    let input = AutoInput {
        tier,
        path,
        body,
        prompt_bytes: bodies.original.len(),
        catalog: &catalog,
        default_model: default_auto_model(&catalog, path),
    };
    let route = state.copilot_auto().route(&upstream, &input).await;
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
    let log = state.selection_log();
    let template = SelectionEvent {
        worker: claude_code_session_id(&bodies.headers)
            .map(|raw| log.scope_key(ScopeKind::Worker, &raw)),
        selector: Some(model_id.qualified()),
        tier: Some(tier.as_str().to_owned()),
        reason: Some(
            if route.swap_reason.is_some() {
                "swapped"
            } else {
                "auto_pick"
            }
            .to_owned(),
        ),
        ..SelectionEvent::new(
            SelectionStage::Solver,
            SelectionState::Selected,
            model_id.qualified(),
        )
    };
    let decision = AutoDecision {
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
        effort: route.effort.map(|level| level.as_str().to_owned()),
    };
    state
        .copilot_auto()
        .decisions()
        .record(TieredDecision { decision, tier });
    Some(AutoPick {
        model_id,
        session_token: route.session_token,
        tier,
        pick: auto_pick,
        reason,
        effort: route.effort,
        template,
    })
}

/// The caller's Claude Code session id, kept only when it is `[A-Za-z0-9._-]{1,128}`,
/// so the log and the decisions API never echo arbitrary header bytes.
fn claude_code_session_id(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(HEADER_CLAUDE_CODE_SESSION_ID)?.as_bytes();
    let valid = (1..=CLAUDE_CODE_SESSION_ID_MAX_LEN).contains(&value.len())
        && value
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    valid.then(|| String::from_utf8_lossy(value).into_owned())
}

/// The salted worker key for the caller's Claude Code session: the same value
/// `selection_events.worker` stores, so a request row joins to its selection events.
/// `None` without a valid session id.
fn worker_key(state: &AppState, headers: &HeaderMap) -> Option<String> {
    claude_code_session_id(headers)
        .map(|raw| state.selection_log().scope_key(ScopeKind::Worker, &raw))
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
    let endpoint = bare_path(path);
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
#[allow(clippy::too_many_arguments)] // fourteen inputs, each a separate piece of one delivery
async fn deliver(
    state: &AppState,
    response: reqwest::Response,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    attempt: &Attempt<'_>,
    client_path: &str,
    client: Option<String>,
    worker: Option<String>,
    commit: Option<SmartAutoCommit>,
    selection: Option<SelectionEvent>,
    prompt_body: Option<Bytes>,
    routed_by: String,
    keep_warm: Option<KeepWarmLease>,
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
            client_path,
            upstream_path,
            model_id,
            attempts_trail,
            started,
            plan,
            client,
            worker,
            commit,
            selection,
            prompt_body,
            routed_by,
        )
        .await
    } else {
        let transform = transforms::response_transform(route, plan, &model_id.qualified(), is_sse);
        build_response(
            state,
            response,
            client_path,
            upstream_path,
            model_id,
            attempts_trail,
            started,
            transform,
            client,
            worker,
            commit,
            selection,
            prompt_body,
            routed_by,
            keep_warm,
        )
    };
    if let Some(applied) = plan.header_value() {
        insert_header(out.headers_mut(), HEADER_TRANSFORMS, &applied);
    }
    out
}

/// True when the attempt is a chat-completions call whose answer review can read.
fn review_applies(attempt: &Attempt<'_>) -> bool {
    !attempt.native_messages && attempt.path.as_str().ends_with(CHAT_ENDPOINT)
}

/// True for a Responses call; the `OpenAI` route keeps the client's `/v1` prefix in its endpoint.
fn is_responses_endpoint(endpoint: &str) -> bool {
    endpoint.trim_start_matches("/v1") == RESPONSES_ENDPOINT
}

/// The reason a `review_failed` selection event carries for a review that ended on `reason`.
fn review_failed_reason(reason: &str) -> String {
    format!("review_failed: {reason}")
}

/// The same request body with streaming off, so review reads the whole answer before replying.
fn non_streaming(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("stream".to_owned(), Value::Bool(false));
        object.remove("stream_options");
    }
    match serde_json::to_vec(&value) {
        Ok(encoded) => Bytes::from(encoded),
        Err(_) => body,
    }
}

/// Buffers a chat-completions answer, reviews it through `ReviewOrchestrator`, and sends the final
/// reply once: a JSON body, or SSE re-emitted in the client's protocol when the client streams.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // the review path threads one request's state; a split would pass the same set again
async fn review_deliver(
    state: &AppState,
    response: reqwest::Response,
    solver: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    route: RouteKind,
    plan: &SmartAutoPlan,
    streams: bool,
    client: Option<String>,
    worker: Option<String>,
    commit: Option<SmartAutoCommit>,
    selection: Option<SelectionEvent>,
    routed_by: String,
) -> Result<Response, AppError> {
    let Some(graph) = plan.review.as_ref() else {
        return Err(AppError::UpstreamUnavailable(
            "review was not planned for this route".to_owned(),
        ));
    };
    let status = response.status().as_u16();
    let solve_reply = async {
        // The whole reply is read even past the byte cap: the orchestrator charges it, finds the budget
        // spent, and delivers it unreviewed.
        let raw = response
            .bytes()
            .await
            .map_err(|e| StageError::Upstream(format!("read error: {e}")))?;
        let completion: Value =
            serde_json::from_slice(&raw).map_err(|e| StageError::Malformed(e.to_string()))?;
        Ok::<StageReply, StageError>(StageReply {
            completion,
            bytes: raw.len() as u64,
        })
    };
    let transport = StageClient::new(state.clone());
    let evidence = ReviewEvidence {
        task: plan.task.as_deref(),
    };
    let outcome = match ReviewOrchestrator::run(
        solve_reply,
        solver,
        graph,
        &transport,
        evidence,
        std::future::pending::<()>(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(event) = &selection {
                state.selection_log().record(SelectionEvent {
                    status: Some(status),
                    reason: Some(review_failed_reason(error.reason())),
                    ..event.in_state(SelectionState::Rejected)
                });
            }
            return Err(AppError::UpstreamUnavailable(format!(
                "review failed: {}",
                error.reason()
            )));
        }
    };
    if outcome.aborted {
        return Err(AppError::UpstreamUnavailable(
            "review was aborted".to_owned(),
        ));
    }
    if let Some(event) = &selection {
        for stage in outcome
            .stages
            .iter()
            .filter(|stage| stage.stage != SelectionStage::Solver)
        {
            state.selection_log().record(SelectionEvent {
                stage: stage.stage,
                model: stage.model.clone(),
                reason: stage
                    .reason
                    .map(str::to_owned)
                    .or_else(|| event.reason.clone()),
                tokens_in: reported(stage.usage.tokens_in),
                tokens_out: reported(stage.usage.tokens_out),
                cache_read_tokens: reported(stage.usage.cache_read),
                cache_write_tokens: reported(stage.usage.cache_write),
                ..event.in_state(stage.state)
            });
        }
        if outcome.rejected {
            state.selection_log().record(SelectionEvent {
                status: Some(status),
                reason: Some(ReviewOrchestrator::REASON_DRAFT_REJECTED.to_owned()),
                ..event.in_state(SelectionState::Rejected)
            });
        } else if let Some(reason) = outcome.review_failed {
            state.selection_log().record(SelectionEvent {
                reason: Some(review_failed_reason(reason)),
                ..event.in_state(SelectionState::Completed)
            });
        }
    }
    if outcome.rejected {
        return Err(AppError::UpstreamUnavailable(
            "review rejected the draft".to_owned(),
        ));
    }

    let usage = outcome.completion.get("usage");
    let count = |pointer: &str| {
        usage
            .and_then(|usage| usage.pointer(pointer))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let request_stats = RequestOutcome {
        profile: solver.profile.clone(),
        model_key: outcome.served.qualified(),
        status,
        ts_ms: 0,
        latency_ms: crate::utils::time::elapsed_ms(started),
        ttft_ms: None,
        tokens_in: count("/prompt_tokens"),
        tokens_out: count("/completion_tokens"),
        cache_read_tokens: count("/prompt_tokens_details/cached_tokens"),
        cache_write_tokens: 0,
        error: None,
        client,
        worker,
        routed_by: Some(routed_by),
    };
    if let Some(commit) = commit {
        commit.apply(state);
    }
    if let Some(attempt) = selection.as_ref().and_then(|event| event.attempt.clone()) {
        let state_for_text = state.clone();
        let completion = outcome.completion.clone();
        tokio::spawn(async move {
            let raw = serde_json::to_vec(&completion).unwrap_or_default();
            save_answer(&state_for_text, &attempt, &raw);
        });
    }
    finish_selection(state, selection, &request_stats, None);
    state.stats().record(&request_stats);
    if let Some(store) = state.store()
        && let Err(e) = store.record(&request_stats)
    {
        tracing::warn!("store record failed: {e}");
    }

    let served = outcome.served.clone();
    let message = match route {
        RouteKind::OpenAi => outcome.completion,
        RouteKind::Anthropic => {
            let raw = serde_json::to_vec(&outcome.completion)
                .map_err(|e| AppError::UpstreamUnavailable(format!("review reply: {e}")))?;
            let rendered = anthropic::response::to_anthropic(status, &raw, &served.qualified());
            serde_json::from_slice::<Value>(&rendered)
                .map_err(|e| AppError::UpstreamUnavailable(format!("review reply: {e}")))?
        }
    };
    let (content_type, body) = if streams {
        let protocol = match route {
            RouteKind::OpenAi => ReplyProtocol::OpenAiChat,
            RouteKind::Anthropic => ReplyProtocol::AnthropicMessage,
        };
        (
            "text/event-stream",
            BufferedReplyStream::render(protocol, &message)?,
        )
    } else {
        let encoded = serde_json::to_vec(&message)
            .map_err(|e| AppError::UpstreamUnavailable(format!("review reply: {e}")))?;
        ("application/json", Bytes::from(encoded))
    };
    let mut out = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .map_err(|e| AppError::UpstreamUnavailable(format!("review reply: {e}")))?;
    insert_header(out.headers_mut(), HEADER_SERVED_MODEL, &served.qualified());
    insert_header(
        out.headers_mut(),
        HEADER_ATTEMPTS,
        &attempts_trail.join(", "),
    );
    Ok(out)
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
    let sender = AttemptSender {
        state,
        method: &method,
        path: upstream_path,
        headers,
        model_id,
        attempt_timeout,
        force_identity_encoding: false,
        session_token: None,
    };
    let response = sender
        .send(body)
        .await
        .map_err(AppError::UpstreamUnavailable)?;
    Ok(build_response(
        state,
        response,
        bare_path(path_and_query),
        upstream_path.as_str(),
        model_id,
        &trail,
        started,
        None,
        client_signature(headers),
        worker_key(state, headers),
        None,
        None,
        None,
        ROUTED_BY_PINNED.to_owned(),
        None,
    ))
}

/// Sends one non-streaming chat-completions body to the concrete model `sent_id` through its
/// pooled client and existing auth. Review stages use it, so they never open a second path.
pub(crate) async fn send_stage(
    state: &AppState,
    sent_id: &ModelId,
    body: Bytes,
) -> Result<reqwest::Response, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let method = Method::POST;
    let sender = AttemptSender {
        state,
        method: &method,
        path: UpstreamPath::Strip(STAGE_CHAT_PATH),
        headers: &headers,
        model_id: sent_id,
        attempt_timeout: None,
        force_identity_encoding: true,
        session_token: None,
    };
    let response = sender.send(body).await?;
    Ok(if may_reply_natively(state, STAGE_CHAT_PATH, sent_id) {
        repair_native_reply(response)
    } else {
        response
    })
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

/// Sends one attempt's body, or a replacement for it, to the attempt's upstream.
struct AttemptSender<'a> {
    state: &'a AppState,
    method: &'a Method,
    path: UpstreamPath<'a>,
    headers: &'a HeaderMap,
    model_id: &'a ModelId,
    attempt_timeout: Option<Duration>,
    /// Set whenever the hub reads the reply itself: `reqwest` has no decoders, so the
    /// caller's `accept-encoding` is replaced (not appended to) with `identity`.
    force_identity_encoding: bool,
    session_token: Option<&'a str>,
}

impl<'a> AttemptSender<'a> {
    fn for_attempt(
        state: &'a AppState,
        method: &'a Method,
        attempt: &Attempt<'a>,
        model_id: &'a ModelId,
        attempt_timeout: Option<Duration>,
        session_token: Option<&'a str>,
    ) -> Self {
        AttemptSender {
            state,
            method,
            path: attempt.path,
            headers: attempt.headers,
            model_id,
            attempt_timeout,
            force_identity_encoding: attempt.force_identity_encoding,
            session_token,
        }
    }

    async fn send(&self, body: Bytes) -> Result<reqwest::Response, String> {
        let state = self.state;
        let config = state.config();
        let profile = config
            .profile(&self.model_id.profile)
            .ok_or_else(|| format!("unknown profile: {}", self.model_id.profile))?;
        if !profile.enabled {
            return Err(format!("profile disabled: {}", profile.name));
        }
        let client = state.client(&profile.name).ok_or("no client for profile")?;
        state.managed().ensure_running(profile).await?;

        let upstream = UpstreamRequest {
            state,
            client: &client,
            profile,
            method: self.method,
            upstream_path: self.path.as_str(),
            headers: self.headers,
            force_identity_encoding: self.force_identity_encoding,
            timeout: self
                .attempt_timeout
                .or(profile.timeout_ms.map(Duration::from_millis)),
            session_token: self.session_token,
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
}

/// Learns the fields a Copilot "Extra inputs are not permitted" 400 names and retries once
/// without them. Any other response comes back unchanged, with `None`. A retry returns the body it sent.
async fn retry_without_rejected_fields(
    bodies: &AttemptBodies<'_>,
    sender: &AttemptSender<'_>,
    response: reqwest::Response,
) -> Result<(reqwest::Response, Option<Bytes>), String> {
    let compressed = is_compressed(response.headers());
    let large = response
        .content_length()
        .is_some_and(|length| length > MAX_REJECTION_BYTES);
    if response.status() != StatusCode::BAD_REQUEST || compressed || large {
        return Ok((response, None));
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
            return Ok((rebuilt_response(status, version, headers, body), None));
        }
    }
    let body = Bytes::from(read);
    let rejected = BodyField::rejected_in(&body);
    if rejected.is_empty() {
        return Ok((rebuilt_response(status, version, headers, body), None));
    }
    let (state, model_id) = (sender.state, sender.model_id);
    state.learn_unsupported_body_fields(&model_id.profile, &rejected);
    let retried = bodies.for_model(state, model_id).body;
    let response = sender.send(retried.clone()).await?;
    Ok((response, Some(retried)))
}

/// The keep-warm lease for a native Copilot Claude attempt on a session, when keep-warm is on.
fn keep_warm_lease(
    state: &AppState,
    attempt: &Attempt<'_>,
    model_id: &ModelId,
    worker: Option<&str>,
) -> Option<KeepWarmLease> {
    let worker = worker?;
    let eligible = attempt.native_messages
        && model_id.model.starts_with(CLAUDE_MODEL_PREFIX)
        && state.config().keep_warm.enabled
        && is_copilot_profile(state, model_id);
    eligible.then(|| state.keep_warm().begin(worker))
}

fn keep_warm_headers(beta: Option<HeaderValue>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(beta) = beta {
        headers.insert(HEADER_ANTHROPIC_BETA, beta);
    }
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(HEADER_X_INITIATOR, HeaderValue::from_static(INITIATOR_AGENT));
    with_anthropic_version(&headers)
}

/// `body` without the fields the profile's upstream is known to reject, as real attempts send it.
fn without_unsupported_fields(state: &AppState, model_id: &ModelId, body: Bytes) -> Bytes {
    let rejected = state.unsupported_body_fields(&model_id.profile);
    if rejected.is_empty() {
        return body;
    }
    let Ok(mut json) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    for field in rejected.iter() {
        field.strip(&mut json);
    }
    serde_json::to_vec(&json).map_or(body, Bytes::from)
}

/// Sends one keep-warm ping to a stream's upstream. It is recorded as a keep-warm request, not a user request.
pub(crate) async fn send_keep_warm_ping(state: &AppState, ping: PingPlan) {
    let PingPlan {
        worker,
        stream_key,
        model_id,
        body,
        beta,
    } = ping;
    let body = without_unsupported_fields(state, &model_id, body);
    let headers = keep_warm_headers(beta);
    let method = Method::POST;
    let sender = AttemptSender {
        state,
        method: &method,
        path: UpstreamPath::Verbatim(MESSAGES_ENDPOINT),
        headers: &headers,
        model_id: &model_id,
        attempt_timeout: None,
        force_identity_encoding: true,
        session_token: None,
    };
    let started = Instant::now();
    let (status, raw, upstream_end) = match sender.send(body).await {
        Ok(response) => {
            let status = response.status().as_u16();
            let (raw, upstream_end) =
                read_upstream_capped(response, MAX_TRANSFORM_BUFFER_BYTES).await;
            (status, raw, upstream_end)
        }
        Err(reason) => (
            StatusCode::BAD_GATEWAY.as_u16(),
            reason.into_bytes(),
            UpstreamEnd::Error,
        ),
    };
    let ok = SUCCESS_STATUS_RANGE.contains(&status) && upstream_end == UpstreamEnd::Complete;
    let usage = scrape_usage(&raw);
    let model_key = model_id.qualified();
    let cost_usd = state.pricing_book().estimate(
        &model_key,
        usage.tokens_in,
        usage.tokens_out,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
    );
    let outcome = RequestOutcome {
        profile: model_id.profile.clone(),
        model_key,
        status,
        ts_ms: now_ms(),
        latency_ms: crate::utils::time::elapsed_ms(started),
        ttft_ms: None,
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        error: (!ok).then(|| error_reason(status, &raw)),
        client: Some(KEEP_WARM_CLIENT.to_string()),
        worker: Some(worker.clone()),
        routed_by: Some(ROUTED_BY_PINNED.to_owned()),
    };
    state.stats().record(&outcome);
    record_request(state, &outcome, None).await;
    state
        .keep_warm()
        .ping_finished(&worker, &stream_key, ok, cost_usd);
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

/// True for a Copilot Claude model sent to `/chat/completions`. Newer ones can
/// answer there in Anthropic shape, which [`repair_native_reply`] converts.
fn may_reply_natively(state: &AppState, upstream_path: &str, sent_id: &ModelId) -> bool {
    bare_path(upstream_path) == ANTHROPIC_UPSTREAM_PATH
        && sent_id.model.starts_with(CLAUDE_MODEL_PREFIX)
        && state
            .config()
            .profile(&sent_id.profile)
            .is_some_and(|profile| profile.copilot.is_some())
}

/// Puts an Anthropic-shaped success reply into `OpenAI` chat-completion shape before
/// anything reads it. An `OpenAI` reply, an error or a compressed body is left alone.
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
    bare_path(path.as_str()) == RESPONSES_ENDPOINT
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
    serves(RESPONSES_ENDPOINT) && !serves(CHAT_ENDPOINT) && !serves(MESSAGES_ENDPOINT)
}

/// Puts a successful Copilot `/responses` reply into Anthropic Messages shape. An error or
/// a compressed body is left alone, so prompt-limit learning still reads it.
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
    let (status, version, headers) = parts_for_rewrite(&response);
    let (raw, upstream_end) = read_upstream_capped(response, MAX_TRANSFORM_BUFFER_BYTES).await;
    if upstream_end == UpstreamEnd::Open {
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
    response.status().is_success() && !is_compressed(response.headers())
}

/// True when the body is encoded; `reqwest` has no decoders, so the hub sees raw bytes.
fn is_compressed(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != IDENTITY_ENCODING)
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
    let (status, version, headers) = parts_for_rewrite(&response);
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
    rebuilt_response(
        status,
        version,
        headers,
        reqwest::Body::wrap_stream(rewritten),
    )
}

/// The status, version and headers a rewritten body keeps; `content-length` no longer holds.
fn parts_for_rewrite(response: &reqwest::Response) -> (StatusCode, reqwest::Version, HeaderMap) {
    let mut headers = response.headers().clone();
    headers.remove(reqwest::header::CONTENT_LENGTH);
    (response.status(), response.version(), headers)
}

/// Copilot serves Anthropic token counting at `/v1/messages/count_tokens`, so a
/// native Claude model keeps the `/v1` that `Strip` would drop.
fn native_count_tokens_path(
    client_path: &str,
    catalog: Option<&UpstreamModel>,
) -> Option<UpstreamPath<'static>> {
    let is_count_tokens = bare_path(client_path) == NATIVE_COUNT_TOKENS_PATH;
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

/// The `/v1/messages` route strips the caller's `anthropic-version`, but the Anthropic API
/// requires one, so a native attempt sends the default.
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

/// One upstream call of `AttemptSender::send`, kept whole so a Copilot 401 can be
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

/// The event a dispatch to `model_id` starts from: the route's template with this attempt's key and effort.
fn selected_event(
    state: &AppState,
    template: &SelectionEvent,
    model_id: &ModelId,
    effort: Option<EffortLevel>,
) -> SelectionEvent {
    SelectionEvent {
        attempt: Some(state.selection_log().attempt_key()),
        model: model_id.qualified(),
        selected_effort: effort,
        sent_effort: effort,
        ..template.clone()
    }
}

/// Records `event` moved to `next`, with the upstream `status` when there is one, and returns the recorded event.
fn record_transition(
    state: &AppState,
    event: &SelectionEvent,
    next: SelectionState,
    status: Option<u16>,
) -> SelectionEvent {
    let moved = SelectionEvent {
        status,
        ..event.in_state(next)
    };
    state.selection_log().record(moved.clone());
    moved
}

/// Records how a delivered attempt ended: completed once the upstream body ended, cancelled when the stream stopped first.
fn finish_selection(
    state: &AppState,
    selection: Option<SelectionEvent>,
    outcome: &RequestOutcome,
    stop: Option<StopCause>,
) {
    let Some(event) = selection else {
        return;
    };
    let next = if stop.is_none() {
        SelectionState::Completed
    } else {
        SelectionState::Cancelled
    };
    state.selection_log().record(SelectionEvent {
        status: Some(outcome.status),
        latency_ms: Some(outcome.latency_ms),
        tokens_in: reported(outcome.tokens_in),
        tokens_out: reported(outcome.tokens_out),
        cache_read_tokens: reported(outcome.cache_read_tokens),
        cache_write_tokens: reported(outcome.cache_write_tokens),
        ..event.in_state(next)
    });
}

/// Why a delivered answer ended before its upstream body did.
#[derive(Clone, Copy)]
enum StopCause {
    /// The response body was dropped first: the client closed it, or the upstream read failed.
    ClientDisconnect,
    /// The hub ended the answer, for the reason it names.
    HubReject(&'static str),
}

/// The client's path and the upstream's, as a request's log lines name them.
#[derive(Clone, Copy)]
struct LogPaths<'a> {
    client: &'a str,
    upstream: &'a str,
}

/// Logs an answer that ended early, with its cause, both paths, and how much reached the client.
fn log_cancel(
    outcome: &RequestOutcome,
    stop: Option<StopCause>,
    paths: LogPaths<'_>,
    bytes_sent: u64,
    upstream_end: UpstreamEnd,
) {
    let Some(cause) = stop else {
        return;
    };
    let (kind, reason) = match cause {
        StopCause::ClientDisconnect => (
            "client_disconnect",
            "the client closed the response or the upstream read failed",
        ),
        StopCause::HubReject(reason) => ("hub_reject", reason),
    };
    tracing::info!(
        model = %outcome.model_key,
        client_path = paths.client,
        upstream_path = paths.upstream,
        latency_ms = outcome.latency_ms,
        bytes_sent,
        upstream_end = upstream_end.as_str(),
        cause = kind,
        "request cancelled: {reason}"
    );
}

/// A count the upstream reported, or `None` when it reported none.
fn reported(count: u64) -> Option<u64> {
    (count > 0).then_some(count)
}

/// Saves the request body of one dispatch as its prompt text. The write runs on a spawned task, off the request path.
fn save_prompt(state: &AppState, selection: &SelectionEvent, body: &Bytes) {
    let Some(attempt) = selection.attempt.clone() else {
        return;
    };
    let prompt = body.slice(..body.len().min(CAPTURE_TEXT_BYTES));
    let state = state.clone();
    tokio::spawn(async move {
        let text = String::from_utf8_lossy(&prompt);
        save_text(&state, |store| {
            store.save_selection_prompt(&attempt, now_ms(), &text)
        });
    });
}

/// Saves the answer of one dispatch from its raw upstream reply, read with `answer_text`. Callers on the request path spawn it.
fn save_answer(state: &AppState, attempt: &str, raw: &[u8]) {
    let text = crate::services::answer_text::answer_text(raw);
    save_text(state, |store| {
        store.save_selection_answer(attempt, now_ms(), &text)
    });
}

/// Runs one text write against the store. A store without text support, or a failed write, is logged and skipped.
fn save_text(
    state: &AppState,
    write: impl FnOnce(&crate::services::store::Store) -> Result<(), String>,
) {
    let Some(store) = state.store() else {
        return;
    };
    if let Err(e) = write(store) {
        tracing::debug!("selection text is not saved: {e}");
    }
}

/// Streams the upstream response through, scraping usage from a rolling tail and recording stats at stream end.
/// `transform` rewrites frames when `Some`; `None` passes bytes through untouched, and `finish()` output ends the body.
// Each input is a separate per-response value, so bundling them would only rename them.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // the stream-end task records one answer in order; a split would thread the same state through helpers
fn build_response(
    state: &AppState,
    upstream: reqwest::Response,
    client_path: &str,
    upstream_path: &str,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    transform: Option<BodyTransform>,
    client: Option<String>,
    worker: Option<String>,
    commit: Option<SmartAutoCommit>,
    selection: Option<SelectionEvent>,
    prompt_body: Option<Bytes>,
    routed_by: String,
    keep_warm: Option<KeepWarmLease>,
) -> Response {
    let status = upstream.status();
    let limit_model =
        prompt_limit_model(state, status, upstream.headers(), model_id, upstream_path);
    let rejected_model =
        rejected_limit_model(state, status, upstream.headers(), model_id, upstream_path);
    // Read before `bytes_stream()` consumes the response. A comment frame
    // injected into a JSON body would corrupt it, so this is SSE-only.
    let keepalive = is_event_stream(&upstream)
        .then(|| state.config().sse_keepalive)
        .flatten();
    let mut response_headers = filter_response_headers(&headermap_from_reqwest(upstream.headers()));
    if transform.is_some() {
        // The rewritten body has no encoding. The attempt asked for `identity`,
        // but the header is stripped rather than trusting the upstream to honour it.
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
    let client_path = client_path.to_owned();
    let path = upstream_path.to_owned();
    let learn_model = model_id.clone();
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
        worker,
        routed_by: Some(routed_by),
    };

    let (summary_sender, summary_receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
    let capture_answer = selection.as_ref().and_then(|event| event.attempt.clone());
    let observed = futures_util::stream::unfold(
        StreamState {
            inner: upstream.bytes_stream(),
            head: Vec::new(),
            tail: Vec::new(),
            transform,
            sender: Some(summary_sender),
            ended: false,
            started,
            ttft_ms: None,
            keepalive,
            upstream_end: UpstreamEnd::Open,
            bytes_sent: 0,
            answer: capture_answer.as_ref().map(|_| Vec::new()),
        },
        pump,
    );

    tokio::spawn(async move {
        let mut summary = summary_receiver.await.unwrap_or_default();
        if let Some(model) = &limit_model {
            state_for_stats.learn_prompt_limit(&outcome_seed.profile, model, &summary.tail);
        }
        if let Some(model) = &rejected_model {
            state_for_stats.learn_rejected_prompt_limit(
                &outcome_seed.profile,
                model,
                &summary.tail,
            );
        }
        learn_image_refusal(
            &state_for_stats,
            &learn_model,
            outcome_seed.status,
            &summary.tail,
        );
        let usage = summary_usage(&summary);
        let answer = std::mem::take(&mut summary.answer);
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
        let logs = LogPaths {
            client: &client_path,
            upstream: &path,
        };
        log_failure(&outcome, logs, summary.bytes_sent, summary.upstream_end);
        if let Some(commit) = commit
            .filter(|_| summary.upstream_end == UpstreamEnd::Complete && outcome.error.is_none())
        {
            commit.apply(&state_for_stats);
        }
        if let Some(lease) = keep_warm
            .filter(|_| summary.upstream_end == UpstreamEnd::Complete && outcome.error.is_none())
        {
            lease.complete(outcome.cache_read_tokens + outcome.cache_write_tokens);
        }
        if let Some(attempt) = capture_answer {
            save_answer(&state_for_stats, &attempt, &answer);
        }
        let stop =
            (summary.upstream_end != UpstreamEnd::Complete).then_some(StopCause::ClientDisconnect);
        log_cancel(
            &outcome,
            stop,
            logs,
            summary.bytes_sent,
            summary.upstream_end,
        );
        finish_selection(&state_for_stats, selection, &outcome, stop);
        state_for_stats.stats().record(&outcome);
        record_request(&state_for_stats, &outcome, prompt_body).await;
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

async fn pump<S>(state: StreamState<S>) -> Option<(Result<Bytes, reqwest::Error>, StreamState<S>)>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let (item, mut state) = pump_step(state).await?;
    if let Ok(bytes) = &item {
        state.bytes_sent += bytes.len() as u64;
    }
    Some((item, state))
}

/// One step of the response body stream: forward or rewrite the next upstream
/// chunk, and on end-of-upstream emit the transform's tail and hand the stats
/// task its summary.
async fn pump_step<S>(
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
                    append_head(&mut state.head, &bytes);
                    append_tail(&mut state.tail, &bytes);
                    if let Some(answer) = state.answer.as_mut() {
                        let room = CAPTURE_TEXT_BYTES.saturating_sub(answer.len());
                        answer.extend_from_slice(&bytes[..room.min(bytes.len())]);
                    }
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
                    state.upstream_end = UpstreamEnd::Error;
                    return Some((Err(e), state));
                }
                NextChunk::Chunk(Err(e)) => {
                    tracing::warn!("upstream stream error, closing translated stream: {e}");
                    state.upstream_end = UpstreamEnd::Error;
                    state.ended = true;
                }
                NextChunk::End => {
                    state.upstream_end = UpstreamEnd::Complete;
                    state.ended = true;
                }
            }
        }

        let tail_chunk = state.transform.as_mut().map(BodyTransform::finish);
        if let Some(sender) = state.sender.take() {
            let tail_bytes = tail_chunk.as_ref().map_or(0, |bytes| bytes.len() as u64);
            let _ = sender.send(StreamSummary {
                head: std::mem::take(&mut state.head),
                tail: std::mem::take(&mut state.tail),
                observed_usage: state
                    .transform
                    .as_ref()
                    .and_then(BodyTransform::observed_usage),
                ttft_ms: state.ttft_ms,
                upstream_end: state.upstream_end,
                bytes_sent: state.bytes_sent + tail_bytes,
                answer: state.answer.take().unwrap_or_default(),
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
    head: Vec<u8>,
    tail: Vec<u8>,
    transform: Option<BodyTransform>,
    sender: Option<tokio::sync::oneshot::Sender<StreamSummary>>,
    ended: bool,
    started: Instant,
    ttft_ms: Option<u64>,
    /// Idle gap after which a keepalive comment goes out. `None` on every
    /// non-SSE body and whenever the knob is off — see `pump`.
    keepalive: Option<Duration>,
    /// How the upstream body ended; `Open` until it does.
    upstream_end: UpstreamEnd,
    /// Bytes handed to the client so far, keepalive comments included.
    bytes_sent: u64,
    /// The upstream bytes kept for the answer text, up to `CAPTURE_TEXT_BYTES`. `None` when the attempt is not in Smart Auto history.
    answer: Option<Vec<u8>>,
}

/// Clients such as Copilot CLI and Claude Code close the connection as soon as
/// they read the final SSE frame, so the body can be dropped before `pump`
/// sees upstream EOF. Hand the stats task what was observed so far instead of
/// letting the sender drop — which recorded 0 tokens and no TTFT.
impl<S> Drop for StreamState<S> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(StreamSummary {
                head: std::mem::take(&mut self.head),
                tail: std::mem::take(&mut self.tail),
                observed_usage: self
                    .transform
                    .as_ref()
                    .and_then(BodyTransform::observed_usage),
                ttft_ms: self.ttft_ms,
                upstream_end: self.upstream_end,
                bytes_sent: self.bytes_sent,
                answer: self.answer.take().unwrap_or_default(),
            });
        }
    }
}

/// What the finished body stream hands the stats task. `observed_usage` wins
/// when a transform reported real numbers — the translated Anthropic body uses
/// `input_tokens`/`output_tokens`, which older scrapers could not see.
#[derive(Default)]
struct StreamSummary {
    head: Vec<u8>,
    tail: Vec<u8>,
    observed_usage: Option<ScrapedUsage>,
    ttft_ms: Option<u64>,
    /// How the upstream body ended. `Complete` means the routed answer is whole.
    upstream_end: UpstreamEnd,
    /// Bytes handed to the client before the stream ended.
    bytes_sent: u64,
    /// The upstream bytes of the answer, cut at `CAPTURE_TEXT_BYTES`. Empty unless the attempt is in Smart Auto history.
    answer: Vec<u8>,
}

/// How an upstream body ended, as the cancel and failure log lines name it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum UpstreamEnd {
    /// The hub stopped reading before the upstream body ended.
    #[default]
    Open,
    /// The upstream body ended on its own.
    Complete,
    /// The upstream read failed.
    Error,
}

impl UpstreamEnd {
    fn as_str(self) -> &'static str {
        match self {
            UpstreamEnd::Open => "open",
            UpstreamEnd::Complete => "complete",
            UpstreamEnd::Error => "error",
        }
    }
}

fn summary_usage(summary: &StreamSummary) -> ScrapedUsage {
    let scraped = scrape_stream_usage(&summary.head, &summary.tail);
    match summary.observed_usage {
        Some(observed) => ScrapedUsage {
            tokens_in: observed.tokens_in,
            tokens_out: observed.tokens_out,
            cache_read_tokens: observed.cache_read_tokens.max(scraped.cache_read_tokens),
            cache_write_tokens: observed.cache_write_tokens.max(scraped.cache_write_tokens),
        },
        None => scraped,
    }
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
// Each input is a separate per-response value the translator needs.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // the buffered answer is read, learned from and recorded in order; a split would thread the same state through helpers
async fn build_anthropic_response(
    state: &AppState,
    upstream: reqwest::Response,
    client_path: &str,
    upstream_path: &str,
    model_id: &ModelId,
    attempts_trail: &[String],
    started: Instant,
    plan: &TransformPlan,
    client: Option<String>,
    worker: Option<String>,
    commit: Option<SmartAutoCommit>,
    selection: Option<SelectionEvent>,
    prompt_body: Option<Bytes>,
    routed_by: String,
) -> Response {
    let upstream_status = upstream.status();
    let limit_model = prompt_limit_model(
        state,
        upstream_status,
        upstream.headers(),
        model_id,
        upstream_path,
    );
    let rejected_model = rejected_limit_model(
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

    let (raw, upstream_end) = read_upstream_capped(upstream, MAX_TRANSFORM_BUFFER_BYTES).await;
    let overflowed = upstream_end == UpstreamEnd::Open;
    if let Some(model) = &limit_model {
        state.learn_prompt_limit(&model_id.profile, model, &raw);
    }
    if let Some(model) = &rejected_model {
        state.learn_rejected_prompt_limit(&model_id.profile, model, &raw);
    }
    learn_image_refusal(state, model_id, upstream_status.as_u16(), &raw);
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
    let bytes_sent = payload.len() as u64;

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
        worker,
        routed_by: Some(routed_by),
    };
    log_failure(
        &outcome,
        LogPaths {
            client: client_path,
            upstream: upstream_path,
        },
        bytes_sent,
        upstream_end,
    );
    let state_for_stats = state.clone();
    let answer_attempt = selection.as_ref().and_then(|event| event.attempt.clone());
    let client_path = client_path.to_owned();
    let path = upstream_path.to_owned();
    tokio::spawn(async move {
        let stop = overflowed.then_some(StopCause::HubReject("response too large to translate"));
        let logs = LogPaths {
            client: &client_path,
            upstream: &path,
        };
        log_cancel(&outcome, stop, logs, bytes_sent, upstream_end);
        finish_selection(&state_for_stats, selection, &outcome, stop);
        if let Some(attempt) = answer_attempt {
            save_answer(&state_for_stats, &attempt, &raw);
        }
        state_for_stats.stats().record(&outcome);
        if let Some(commit) = commit.filter(|_| outcome.error.is_none()) {
            commit.apply(&state_for_stats);
        }
        record_request(&state_for_stats, &outcome, prompt_body).await;
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

/// The upstream model id a Copilot 400 on `upstream_path` may teach a stored prompt limit for,
/// read before the body streams. A compressed body is skipped.
fn prompt_limit_model(
    state: &AppState,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    model_id: &ModelId,
    upstream_path: &str,
) -> Option<String> {
    if !is_copilot_profile(state, model_id) {
        return None;
    }
    rejected_limit_model(state, status, headers, model_id, upstream_path)
}

/// The upstream model id a 400 on `upstream_path` may teach an in-memory prompt limit for, read
/// before the body streams. A Copilot profile teaches only on the paths that speak for its stored limit.
fn rejected_limit_model(
    state: &AppState,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    model_id: &ModelId,
    upstream_path: &str,
) -> Option<String> {
    if status != StatusCode::BAD_REQUEST || is_compressed(headers) {
        return None;
    }
    let upstream_id = upstream_key(state, model_id);
    (!is_copilot_profile(state, model_id) || teaches_stored_limit(upstream_path, &upstream_id))
        .then_some(upstream_id)
}

fn is_copilot_profile(state: &AppState, model_id: &ModelId) -> bool {
    state
        .config()
        .profile(&model_id.profile)
        .is_some_and(|found| found.copilot.is_some())
}

/// True when a rejection on `upstream_path` speaks for `upstream_id`'s stored limit: native
/// `/v1/messages` and `count_tokens` for Claude, `/responses` for any other model.
fn teaches_stored_limit(upstream_path: &str, upstream_id: &str) -> bool {
    let path = bare_path(upstream_path);
    if upstream_id.starts_with(CLAUDE_MODEL_PREFIX) {
        path == MESSAGES_ENDPOINT || path == NATIVE_COUNT_TOKENS_PATH
    } else {
        path == RESPONSES_ENDPOINT
    }
}

/// Reads an upstream body into memory, stopping once `cap` is exceeded.
/// A read error ends the read with whatever arrived, which the translator then renders as an error.
async fn read_upstream_capped(upstream: reqwest::Response, cap: usize) -> (Vec<u8>, UpstreamEnd) {
    let mut stream = upstream.bytes_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else {
            return (buffered, UpstreamEnd::Error);
        };
        buffered.extend_from_slice(&bytes);
        if buffered.len() > cap {
            return (buffered, UpstreamEnd::Open);
        }
    }
    (buffered, UpstreamEnd::Complete)
}

/// Writes the request row, then the prompt shape of an Anthropic request beside it. The json store
/// returns no row id, so it writes no shape.
async fn record_request(state: &AppState, outcome: &RequestOutcome, prompt_body: Option<Bytes>) {
    let Some(store) = state.store() else {
        return;
    };
    match store.record(outcome) {
        Ok(Some(request_id)) => {
            if let Some(body) = prompt_body {
                record_prompt_shape(state.clone(), request_id, body).await;
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("store record failed: {e}"),
    }
}

/// Reads the shape in `spawn_blocking`, off the runtime threads, then writes it for `request_id`.
async fn record_prompt_shape(state: AppState, request_id: i64, body: Bytes) {
    let written = tokio::task::spawn_blocking(move || {
        let (Some(shape), Some(store)) = (PromptShape::from_body(&body), state.store()) else {
            return Ok(());
        };
        store.record_shape(request_id, &shape)
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("store record shape failed: {e}"),
        Err(e) => tracing::warn!("prompt shape task failed: {e}"),
    }
}

#[allow(clippy::too_many_arguments)] // the failure record takes the attempt's values one by one, as the attempt loop holds them
fn record_failure(
    state: &AppState,
    model_id: &ModelId,
    status: u16,
    started: Instant,
    reason: String,
    client: Option<String>,
    worker: Option<String>,
    routed_by: String,
    paths: LogPaths<'_>,
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
        worker,
        routed_by: Some(routed_by),
    };
    let upstream_end = if status == 0 {
        UpstreamEnd::Error
    } else {
        UpstreamEnd::Complete
    };
    log_failure(&outcome, paths, 0, upstream_end);
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
        bare_path(path_and_query),
        upstream_path,
        &model_id,
        &[model_id.qualified()],
        started,
        None,
        client_signature(headers),
        worker_key(state, headers),
        None,
        None,
        None,
        ROUTED_BY_PINNED.to_owned(),
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

fn log_failure(
    outcome: &RequestOutcome,
    paths: LogPaths<'_>,
    bytes_sent: u64,
    upstream_end: UpstreamEnd,
) {
    if let Some(reason) = &outcome.error {
        tracing::warn!(
            model = %outcome.model_key,
            client_path = paths.client,
            upstream_path = paths.upstream,
            status = outcome.status,
            latency_ms = outcome.latency_ms,
            bytes_sent,
            upstream_end = upstream_end.as_str(),
            "request failed: {reason}"
        );
    }
}

fn append_head(head: &mut Vec<u8>, bytes: &Bytes) {
    let room = USAGE_SCRAPE_HEAD_BYTES.saturating_sub(head.len());
    head.extend_from_slice(&bytes[..room.min(bytes.len())]);
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use futures_util::stream::{self, BoxStream, StreamExt as _};

    use super::*;
    use crate::services::store::Store;

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
            head: Vec::new(),
            tail: Vec::new(),
            transform: None,
            sender,
            ended: false,
            started: Instant::now(),
            ttft_ms: None,
            keepalive,
            upstream_end: UpstreamEnd::Open,
            bytes_sent: 0,
            answer: None,
        }
    }

    #[test]
    fn a_routed_commit_needs_a_2xx_status() {
        let signals = TurnSignals::from_body(
            "/chat/completions",
            &serde_json::json!({}),
            0,
            Some("session-1".to_owned()),
        );
        let plan = SmartAutoPlan {
            key: AffinityKey::new(
                &signals,
                &SmartAutoHeaders::default(),
                &SmartAutoPolicy::default(),
                "/chat/completions",
            ),
            tier: SmartAutoTier::Balance,
            role: WorkRole::Conversation,
            plan_backed: false,
            turn: 1,
            requested: None,
            model: "alpha/m1".to_owned(),
            effort: None,
            reason: RouteReason::Fresh,
            review: None,
            task: None,
            template: SelectionEvent::new(
                SelectionStage::Solver,
                SelectionState::Selected,
                "alpha/m1",
            ),
        };
        let served = ModelId {
            profile: "alpha".to_owned(),
            model: "m1".to_owned(),
        };

        assert!(plan.commit(500, &served).is_none());
        assert!(plan.commit(429, &served).is_none());
        assert_eq!(
            plan.commit(200, &served).map(|commit| commit.model),
            Some("alpha/m1".to_owned())
        );
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

    /// A native `/v1/messages` stream reports input and cache usage only in
    /// `message_start`. The final `message_delta` carries output tokens only.
    fn long_native_messages_stream() -> Vec<reqwest::Result<Bytes>> {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus\",\"content\":[],\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":30,\"cache_read_input_tokens\":900,\"output_tokens\":1}}}\n\n";
        let delta = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lorem ipsum dolor sit amet \"}}\n\n";
        let end = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4000}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let delta_count = USAGE_SCRAPE_TAIL_BYTES / delta.len() + 1;
        std::iter::once(start)
            .chain(std::iter::repeat_n(delta, delta_count))
            .chain(std::iter::once(end))
            .map(|frame| Ok(Bytes::from_static(frame.as_bytes())))
            .collect()
    }

    #[tokio::test]
    async fn long_native_messages_stream_keeps_message_start_cache_usage() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
        let upstream = stream::iter(long_native_messages_stream()).boxed();

        drain(state_for(upstream, None, Some(sender))).await;
        let usage = summary_usage(&receiver.await.unwrap());

        assert_eq!(
            usage,
            ScrapedUsage {
                tokens_in: 12,
                tokens_out: 4000,
                cache_read_tokens: 900,
                cache_write_tokens: 30,
            }
        );
    }

    #[test]
    fn no_eligible_model_names_every_restriction() {
        let selector = HubSelector::parse("hub/smart-auto-fast", &BTreeMap::new())
            .unwrap()
            .unwrap();
        let policy = SmartAutoPolicy {
            include: vec![BTreeSet::from([
                "alpha/*".to_string(),
                "beta/*".to_string(),
            ])],
            exclude: BTreeSet::from(["*preview*".to_string()]),
            min_context_tokens: Some(1_000_000),
            ..SmartAutoPolicy::default()
        };
        let profiles = ProfileFilter::from_list("beta,gamma");

        let error = no_eligible_model(&selector, &policy, Some(&profiles), "/v1/messages", false);

        let AppError::BadRequest(message) = error else {
            panic!("expected a 400, got {error:?}");
        };
        assert_eq!(
            message,
            "smart_auto_no_eligible_model: no model serves hub/smart-auto-fast on /v1/messages \
             under: tier fast, profiles beta|gamma, include alpha/*|beta/*, exclude *preview*, \
             min context 1000000 tokens"
        );
    }

    #[test]
    fn no_eligible_model_names_an_image_request() {
        let selector = HubSelector::parse("hub/smart-auto", &BTreeMap::new())
            .unwrap()
            .unwrap();

        let error = no_eligible_model(
            &selector,
            &SmartAutoPolicy::default(),
            None,
            "/v1/messages",
            true,
        );

        let AppError::BadRequest(message) = error else {
            panic!("expected a 400, got {error:?}");
        };
        assert_eq!(
            message,
            "smart_auto_no_eligible_model: no model serves hub/smart-auto on /v1/messages \
             under: tier balance, image input"
        );
    }

    #[test]
    fn routed_by_label_names_who_chose_the_model() {
        assert_eq!(routed_by_label(true, None), "copilot-auto");
        assert_eq!(
            routed_by_label(false, Some(SmartAutoTier::Balance)),
            "smart-auto:balance"
        );
        assert_eq!(
            routed_by_label(false, Some(SmartAutoTier::Intelligence)),
            "smart-auto:intelligence"
        );
        assert_eq!(routed_by_label(false, None), "pinned");
    }

    #[test]
    fn profile_filter_restricts_the_pool_only_when_it_names_a_concrete_profile() {
        let filter = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(ProfileFilter::HEADER, value.parse().unwrap());
            concrete_profile_filter(&headers).map(|filter| filter.to_list())
        };

        assert_eq!(filter("hub"), None);
        assert_eq!(filter(" "), None);
        assert_eq!(filter("hub, Beta"), Some("hub,beta".to_string()));
        assert_eq!(concrete_profile_filter(&HeaderMap::new()), None);
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

    #[tokio::test]
    async fn a_drained_stream_reports_a_complete_upstream_and_the_bytes_sent() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
        let upstream = stream::iter(vec![Ok(Bytes::from_static(b"data: one\n\n"))]).boxed();

        drain(state_for(upstream, None, Some(sender))).await;
        let summary = receiver.await.unwrap();

        assert_eq!(summary.upstream_end, UpstreamEnd::Complete);
        assert_eq!(summary.bytes_sent, "data: one\n\n".len() as u64);
    }

    #[tokio::test]
    async fn a_stream_dropped_mid_body_reports_an_open_upstream_and_the_bytes_sent() {
        let (sender, receiver) = tokio::sync::oneshot::channel::<StreamSummary>();
        let upstream = stream::iter(vec![
            Ok(Bytes::from_static(b"data: one\n\n")),
            Ok(Bytes::from_static(b"data: two\n\n")),
        ])
        .boxed();
        let (first, state) = pump(state_for(upstream, None, Some(sender))).await.unwrap();
        assert_eq!(first.unwrap(), "data: one\n\n");

        drop(state);
        let summary = receiver.await.unwrap();

        assert_eq!(summary.upstream_end, UpstreamEnd::Open);
        assert_eq!(summary.bytes_sent, "data: one\n\n".len() as u64);
    }

    mod copilot_routing {
        use std::collections::HashMap;
        use std::sync::Arc;

        use serde_json::json;
        use wiremock::matchers::{body_partial_json, method, path};
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
            copilot_with_store(None).await
        }

        async fn copilot_with_store(sqlite: Option<Store>) -> Copilot {
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
                sqlite,
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
                RouteKind::Anthropic => MESSAGES_ENDPOINT,
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
                RouteKind::Anthropic => MESSAGES_ENDPOINT,
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
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
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

        const CLAUDE_SESSION: &str = "keep-warm-session";
        const CACHED_NATIVE_REPLY: &str = r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"native"}],"usage":{"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":4096}}"#;
        const CACHED_CHAT_REPLY: &str = r#"{"id":"chatcmpl-1","choices":[{"index":0,"message":{"role":"assistant","content":"translated"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4100,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":4096}}}"#;

        async fn mount_cached_native_reply(copilot: &Copilot) {
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(CACHED_NATIVE_REPLY.as_bytes(), "application/json"),
                )
                .mount(&copilot.upstream)
                .await;
        }

        fn keep_warm_cap() -> Duration {
            Duration::from_mins(30)
        }

        /// Polls for a due keep-warm ping, as the loop does once the stream has been idle long enough.
        async fn due_pings(copilot: &Copilot) -> Vec<crate::services::keep_warm::PingPlan> {
            for _ in 0..200 {
                let now = std::time::Instant::now() + crate::consts::KEEP_WARM_PING_INTERVAL;
                let plans = copilot.state.keep_warm().take_due(now, keep_warm_cap());
                if !plans.is_empty() {
                    return plans;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Vec::new()
        }

        #[tokio::test]
        async fn native_claude_stream_is_kept_for_the_keep_warm_loop() {
            let copilot = copilot().await;
            mount_cached_native_reply(&copilot).await;
            let body = anthropic_body(NATIVE_ID, false);

            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &body,
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let plans = due_pings(&copilot).await;
            assert_eq!(plans.len(), 1);
            assert_eq!(plans[0].model_id.model, NATIVE_ID);
        }

        #[tokio::test]
        async fn requests_that_are_not_native_claude_keep_no_stream() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(CACHED_CHAT_REPLY.as_bytes(), "application/json"),
                )
                .mount(&copilot.upstream)
                .await;
            let translated = anthropic_body(TRANSLATED_ID, false);
            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &translated,
                RouteKind::Anthropic,
            )
            .await;
            assert_eq!(status, 200);
            let plain = json!({
                "model": format!("{PROFILE}/{PLAIN_ID}"),
                "messages": [{"role": "user", "content": "hi"}],
            });
            let (status, _) = send(
                &copilot,
                &with_session(CLAUDE_SESSION),
                &plain,
                RouteKind::OpenAi,
            )
            .await;
            assert_eq!(status, 200);

            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = std::time::Instant::now() + crate::consts::KEEP_WARM_PING_INTERVAL;
            assert!(
                copilot
                    .state
                    .keep_warm()
                    .take_due(now, keep_warm_cap())
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn rejected_field_retry_keeps_the_body_that_was_accepted() {
            let copilot = copilot().await;
            let rejected =
                json!({"error": {"message": "context_management: Extra inputs are not permitted"}});
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(ResponseTemplate::new(400).set_body_json(rejected))
                .up_to_n_times(1)
                .mount(&copilot.upstream)
                .await;
            mount_cached_native_reply(&copilot).await;
            let mut body = anthropic_body(NATIVE_ID, false);
            body["context_management"] = json!({"edits": []});

            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &body,
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent.len(), 2);
            assert!(sent_json(&sent[0]).get("context_management").is_some());
            let plans = due_pings(&copilot).await;
            assert_eq!(plans.len(), 1);
            let kept: Value = serde_json::from_slice(&plans[0].body).unwrap();
            assert!(kept.get("context_management").is_none());
            assert_eq!(kept["model"], sent_json(&sent[1])["model"]);
        }

        #[tokio::test]
        async fn keep_warm_ping_replays_the_stored_body_and_records_a_keep_warm_row() {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("hub.db");
            let copilot = copilot_with_store(Some(
                Store::open("sqlite", Some(db.to_str().unwrap())).unwrap(),
            ))
            .await;
            mount_cached_native_reply(&copilot).await;
            let body = anthropic_body(NATIVE_ID, false);
            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &body,
                RouteKind::Anthropic,
            )
            .await;
            assert_eq!(status, 200);
            let plan = due_pings(&copilot).await.into_iter().next().unwrap();

            send_keep_warm_ping(&copilot.state, plan).await;

            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[1].headers.get("x-initiator").unwrap(), "agent");
            let mut expected = body.clone();
            expected["model"] = json!(NATIVE_UPSTREAM_ID);
            expected["stream"] = json!(false);
            expected["max_tokens"] = json!(1);
            assert_eq!(sent_json(&sent[1]), expected);
            assert_eq!(copilot.state.keep_warm().ping_totals().0, 1);
            let keep_warm_rows = count_until(
                &db,
                "SELECT COUNT(*) FROM requests WHERE client = 'llm-hub/keep-warm'",
                1,
            )
            .await;
            assert_eq!(keep_warm_rows, 1);
        }

        #[tokio::test]
        async fn keep_warm_ping_drops_fields_the_upstream_rejects() {
            let copilot = copilot().await;
            mount_cached_native_reply(&copilot).await;
            let mut body = anthropic_body(NATIVE_ID, false);
            body["context_management"] = json!({"edits": []});
            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &body,
                RouteKind::Anthropic,
            )
            .await;
            assert_eq!(status, 200);
            copilot.state.learn_unsupported_body_fields(
                PROFILE,
                &[BodyField::TopLevel("context_management".to_string())],
            );
            let plan = due_pings(&copilot).await.into_iter().next().unwrap();

            send_keep_warm_ping(&copilot.state, plan).await;

            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent.len(), 2);
            assert!(sent_json(&sent[1]).get("context_management").is_none());
        }

        #[tokio::test]
        async fn keep_warm_ping_sends_the_client_anthropic_beta() {
            let copilot = copilot().await;
            mount_cached_native_reply(&copilot).await;
            let mut headers = with_caller_version(with_session(CLAUDE_SESSION));
            headers.insert(
                HEADER_ANTHROPIC_BETA,
                HeaderValue::from_static("demo-beta-2025-01-01"),
            );
            let (status, _) = send(
                &copilot,
                &headers,
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;
            assert_eq!(status, 200);
            let plan = due_pings(&copilot).await.into_iter().next().unwrap();

            send_keep_warm_ping(&copilot.state, plan).await;

            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent.len(), 2);
            assert_eq!(
                sent[1].headers.get(HEADER_ANTHROPIC_BETA).unwrap(),
                "demo-beta-2025-01-01"
            );
        }

        #[tokio::test]
        async fn failed_keep_warm_ping_drops_its_stream() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(CACHED_NATIVE_REPLY.as_bytes(), "application/json"),
                )
                .up_to_n_times(1)
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(ResponseTemplate::new(500).set_body_raw("overloaded", "text/plain"))
                .mount(&copilot.upstream)
                .await;
            let body = anthropic_body(NATIVE_ID, false);
            let (status, _) = send(
                &copilot,
                &with_caller_version(with_session(CLAUDE_SESSION)),
                &body,
                RouteKind::Anthropic,
            )
            .await;
            assert_eq!(status, 200);
            let plan = due_pings(&copilot).await.into_iter().next().unwrap();

            send_keep_warm_ping(&copilot.state, plan).await;

            let now = std::time::Instant::now() + crate::consts::KEEP_WARM_PING_INTERVAL * 2;
            assert!(
                copilot
                    .state
                    .keep_warm()
                    .take_due(now, keep_warm_cap())
                    .is_empty()
            );
            assert_eq!(copilot.state.keep_warm().ping_totals().0, 1);
        }

        const SSE_REPLY: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        /// Reads one count from the sqlite file, retrying while the stream-end task writes.
        async fn count_until(db: &std::path::Path, sql: &str, expected: i64) -> i64 {
            let conn = rusqlite::Connection::open(db).unwrap();
            conn.busy_timeout(Duration::from_secs(1)).unwrap();
            let mut count = 0;
            for _ in 0..200 {
                count = conn.query_row(sql, [], |row| row.get(0)).unwrap();
                if count == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            count
        }

        #[tokio::test]
        async fn anthropic_sse_request_writes_one_shape_row() {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("hub.db");
            let copilot = copilot_with_store(Some(
                Store::open("sqlite", Some(db.to_str().unwrap())).unwrap(),
            ))
            .await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_raw(SSE_REPLY.as_bytes(), "text/event-stream"),
                )
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &with_caller_version(client_headers()),
                &anthropic_body(NATIVE_ID, true),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(
                count_until(&db, "SELECT COUNT(*) FROM request_shapes", 1).await,
                1
            );
            assert_eq!(
                count_until(
                    &db,
                    "SELECT COUNT(*) FROM request_shapes s JOIN requests r ON r.id = s.request_id",
                    1,
                )
                .await,
                1
            );
            let conn = rusqlite::Connection::open(&db).unwrap();
            let (messages, turn_start): (i64, i64) = conn
                .query_row(
                    "SELECT messages, turn_start FROM request_shapes",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((messages, turn_start), (1, 1));
        }

        #[tokio::test]
        async fn openai_route_request_writes_no_shape_row() {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("hub.db");
            let copilot = copilot_with_store(Some(
                Store::open("sqlite", Some(db.to_str().unwrap())).unwrap(),
            ))
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

            let (status, _) = send(&copilot, &client_headers(), &body, RouteKind::OpenAi).await;

            assert_eq!(status, 200);
            assert_eq!(
                count_until(&db, "SELECT COUNT(*) FROM requests", 1).await,
                1
            );
            assert_eq!(
                count_until(&db, "SELECT COUNT(*) FROM request_shapes", 0).await,
                0
            );
        }

        #[tokio::test]
        async fn pinned_request_records_routed_by_pinned() {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("hub.db");
            let copilot = copilot_with_store(Some(
                Store::open("sqlite", Some(db.to_str().unwrap())).unwrap(),
            ))
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

            let (status, _) = send(&copilot, &client_headers(), &body, RouteKind::OpenAi).await;

            assert_eq!(status, 200);
            assert_eq!(
                count_until(
                    &db,
                    "SELECT COUNT(*) FROM requests WHERE routed_by = 'pinned'",
                    1
                )
                .await,
                1
            );
        }

        #[tokio::test]
        async fn native_attempt_sends_the_default_version_when_the_caller_sent_none() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
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

            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
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
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = sent_json(&requests_to(&copilot, MESSAGES_ENDPOINT).await[0]);
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
                .and(path(MESSAGES_ENDPOINT))
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
                .and(path(RESPONSES_ENDPOINT))
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
                .and(path(MESSAGES_ENDPOINT))
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
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .with_priority(10)
                .mount(&copilot.upstream)
                .await;
        }

        #[tokio::test]
        async fn rejected_top_level_field_is_learned_retried_and_never_sent_again() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
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
                .and(path(MESSAGES_ENDPOINT))
                .and(has_context_management)
                .and(asks_for_compression)
                .respond_with(
                    extra_input("context_management").insert_header("content-encoding", "gzip"),
                )
                .with_priority(1)
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
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
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent.len(), 2);
            assert!(!has_output_config(&sent[1]), "{}", sent_json(&sent[1]));
            assert_eq!(sent_json(&sent[1])["messages"].as_array().unwrap().len(), 2);
        }

        #[tokio::test]
        async fn unrelated_native_400_is_neither_retried_nor_learned() {
            let copilot = copilot().await;
            let error = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: Field required"}}"#;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
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
            assert_eq!(requests_to(&copilot, MESSAGES_ENDPOINT).await.len(), 1);
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

            let native = requests_to(&copilot, MESSAGES_ENDPOINT).await;
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
            assert!(requests_to(&copilot, MESSAGES_ENDPOINT).await.is_empty());
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
                .and(path(MESSAGES_ENDPOINT))
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
            assert_eq!(requests_to(&copilot, MESSAGES_ENDPOINT).await.len(), 1);
            let chat = requests_to(&copilot, UPSTREAM_CHAT_PATH).await;
            assert_eq!(sent_json(&chat[0])["model"], PLAIN_ID);
        }

        const AUTO_SESSION: &str = "auto-session";
        const RESPONSES_ONLY_ID: &str = "gpt-5.5";
        const CHAT_ONLY_ID: &str = "gemini-3-pro";

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
                .and(path(MESSAGES_ENDPOINT))
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
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent_json(&sent[0])["model"], NATIVE_UPSTREAM_ID);
            assert_eq!(session_tokens(&sent[0]), [AUTO_SESSION]);
        }

        #[tokio::test]
        async fn auto_on_v1_responses_sends_the_selected_model_with_the_session_token() {
            let copilot = copilot().await;
            mount_auto(&copilot, RESPONSES_ONLY_ID).await;
            Mock::given(method("POST"))
                .and(path(RESPONSES_ENDPOINT))
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
            let sent = requests_to(&copilot, RESPONSES_ENDPOINT).await;
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
            assert_eq!(header(&headers, HEADER_AUTO_TIER), None);
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
                .and(path(MESSAGES_ENDPOINT))
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
            assert_eq!(header(&headers, HEADER_AUTO_TIER), Some("balance"));
            let decisions = copilot
                .state
                .copilot_auto()
                .decisions()
                .recent(Some(session), 1);
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0].tier, AutoTier::Balance);
            let decision = &decisions[0].decision;
            assert_eq!(decision.session.as_deref(), Some(session));
            assert_eq!(decision.profile, PROFILE);
            assert_eq!(decision.auto_pick, auto_pick);
            assert_eq!(decision.model, served);
            assert_eq!(decision.swap_reason.as_deref(), Some(reason.as_str()));
            assert_eq!(decision.selection_reason.as_deref(), Some("test pick"));
            assert_eq!(decision.turn, AutoTurn::Routed);
        }

        #[tokio::test]
        async fn copilot_auto_request_records_routed_by_copilot_auto() {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("hub.db");
            let copilot = copilot_with_store(Some(
                Store::open("sqlite", Some(db.to_str().unwrap())).unwrap(),
            ))
            .await;
            mount_auto(&copilot, CHAT_ONLY_ID).await;
            mount_native_reply(&copilot).await;

            let _ = send_for_headers(
                &copilot,
                &with_session("routed-by-session"),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(
                count_until(
                    &db,
                    "SELECT COUNT(*) FROM requests WHERE routed_by = 'copilot-auto'",
                    1
                )
                .await,
                1
            );
        }

        #[tokio::test]
        async fn auto_tier_model_routes_on_its_tier_and_names_it_in_header_and_log() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            mount_native_reply(&copilot).await;

            let headers = send_for_headers(
                &copilot,
                &client_headers(),
                &anthropic_body("auto-max", false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(header(&headers, HEADER_AUTO_TIER), Some("intelligence"));
            let auto = requests_to(&copilot, "/auto").await;
            assert_eq!(sent_json(&auto[0])["tier"], "intelligence");
            let sent = requests_to(&copilot, MESSAGES_ENDPOINT).await;
            assert_eq!(sent_json(&sent[0])["model"], NATIVE_UPSTREAM_ID);
            let decisions = copilot.state.copilot_auto().decisions().recent(None, 10);
            assert_eq!(decisions[0].tier, AutoTier::Intelligence);
        }

        /// Polls the selection log until it holds `count` events. A stream's end is recorded by the
        /// stats task, which can run after the body has been read.
        async fn selection_events(copilot: &Copilot, count: usize) -> Vec<SelectionEvent> {
            use crate::schemas::history_query::HistoryQuery;

            for _ in 0..200 {
                let events = copilot.state.selection_log().history(&HistoryQuery {
                    since_seq: 0,
                    limit: usize::MAX,
                    ..HistoryQuery::default()
                });
                if events.len() >= count {
                    return events;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("the selection log holds fewer than {count} events");
        }

        fn states(events: &[SelectionEvent]) -> Vec<SelectionState> {
            events.iter().map(|event| event.state).collect()
        }

        #[tokio::test]
        async fn auto_success_records_selected_started_then_completed() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            mount_native_reply(&copilot).await;

            let (status, _) = send(
                &copilot,
                &with_session("selection-success"),
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let events = selection_events(&copilot, 3).await;
            assert_eq!(
                states(&events),
                [
                    SelectionState::Selected,
                    SelectionState::Started,
                    SelectionState::Completed,
                ]
            );
            assert!(
                events
                    .iter()
                    .all(|event| event.model == format!("{PROFILE}/{NATIVE_ID}"))
            );
            assert_eq!(events[0].tier.as_deref(), Some("balance"));
            assert!(events[0].worker.is_some());
            assert_eq!(events[2].status, Some(200));
            assert_eq!(events[0].attempt, events[2].attempt);
        }

        #[tokio::test]
        async fn auto_fallback_records_the_rejection_then_the_served_attempt() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
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
            let mut headers = with_session("selection-fallback");
            headers.insert(
                HEADER_FALLBACKS,
                HeaderValue::from_str(&format!("{PROFILE}/{PLAIN_ID}")).unwrap(),
            );

            let (status, _) = send(
                &copilot,
                &headers,
                &anthropic_body(COPILOT_AUTO_MODEL, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            let events = selection_events(&copilot, 5).await;
            assert_eq!(
                states(&events),
                [
                    SelectionState::Selected,
                    SelectionState::Rejected,
                    SelectionState::Selected,
                    SelectionState::Started,
                    SelectionState::Completed,
                ]
            );
            let models: Vec<&str> = events.iter().map(|event| event.model.as_str()).collect();
            let native = format!("{PROFILE}/{NATIVE_ID}");
            let plain = format!("{PROFILE}/{PLAIN_ID}");
            assert_eq!(
                models,
                [
                    native.as_str(),
                    native.as_str(),
                    plain.as_str(),
                    plain.as_str(),
                    plain.as_str(),
                ]
            );
            assert_eq!(events[1].status, Some(429));
        }

        #[tokio::test]
        async fn auto_stream_the_client_drops_records_cancelled() {
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            Mock::given(method("POST"))
                .and(path(MESSAGES_ENDPOINT))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    "event: message_start\ndata: {\"type\":\"message_start\"}\n\n".as_bytes(),
                    "text/event-stream",
                ))
                .mount(&copilot.upstream)
                .await;

            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                MESSAGES_ENDPOINT,
                &with_session("selection-drop"),
                Bytes::from(anthropic_body(COPILOT_AUTO_MODEL, true).to_string()),
                RouteKind::Anthropic,
            )
            .await
            .unwrap();
            drop(response);

            let events = selection_events(&copilot, 3).await;
            assert_eq!(
                states(&events),
                [
                    SelectionState::Selected,
                    SelectionState::Started,
                    SelectionState::Cancelled,
                ]
            );
            assert!(events[2].latency_ms.is_some());
        }

        /// The bytes a tracing subscriber writes, shared between the subscriber and the test.
        #[derive(Clone, Default)]
        struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogBuffer {
            type Writer = LogBuffer;

            fn make_writer(&'writer self) -> LogBuffer {
                self.clone()
            }
        }

        #[tokio::test]
        async fn a_cancelled_stream_logs_the_client_path_and_the_upstream_path() {
            let logs = LogBuffer::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);
            let copilot = copilot().await;
            mount_auto(&copilot, NATIVE_UPSTREAM_ID).await;
            let frames = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(frames.as_bytes(), "text/event-stream"),
                )
                .mount(&copilot.upstream)
                .await;
            let body = json!({
                "model": format!("{PROFILE}/{COPILOT_AUTO_MODEL}"),
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true,
            });

            let response = attempt_loop(
                &copilot.state,
                Method::POST,
                "/v1/chat/completions",
                &with_session("cancel-log"),
                Bytes::from(body.to_string()),
                RouteKind::OpenAi,
            )
            .await
            .unwrap();
            let mut answer = response.into_body().into_data_stream();
            let first = futures_util::StreamExt::next(&mut answer)
                .await
                .unwrap()
                .unwrap();
            drop(answer);

            selection_events(&copilot, 3).await;
            let written = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
            assert!(
                written.contains("client_path=\"/v1/chat/completions\""),
                "{written}"
            );
            assert!(
                written.contains("upstream_path=\"/chat/completions\""),
                "{written}"
            );
            assert!(
                written.contains(&format!("bytes_sent={}", first.len())),
                "{written}"
            );
            assert!(written.contains("upstream_end=\"open\""), "{written}");
        }

        #[tokio::test]
        async fn direct_model_records_no_selection() {
            let copilot = copilot().await;
            mount_native_reply(&copilot).await;

            let (status, _) = send(
                &copilot,
                &client_headers(),
                &anthropic_body(NATIVE_ID, false),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert!(
                copilot
                    .state
                    .selection_log()
                    .history(&crate::schemas::history_query::HistoryQuery {
                        limit: usize::MAX,
                        ..Default::default()
                    })
                    .is_empty()
            );
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
            assert_eq!(decisions[0].decision.session, None);
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
            let sent = requests_to(&copilot, RESPONSES_ENDPOINT).await;
            assert_eq!(sent_json(&sent[0])["model"], RESPONSES_ONLY_ID);
            assert_eq!(session_tokens(&sent[0]), [AUTO_SESSION]);
            assert!(requests_to(&copilot, MESSAGES_ENDPOINT).await.is_empty());
        }

        fn list_responses_only_model(copilot: &Copilot) {
            copilot.state.set_copilot_catalog(
                PROFILE,
                vec![json!({"id": RESPONSES_ONLY_ID, "supported_endpoints": ["/responses"]})],
            );
        }

        async fn mount_responses_reply(copilot: &Copilot, reply: &Value) {
            Mock::given(method("POST"))
                .and(path(RESPONSES_ENDPOINT))
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
            let sent = requests_to(&copilot, RESPONSES_ENDPOINT).await;
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
                .and(path(RESPONSES_ENDPOINT))
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
            let sent = requests_to(&copilot, RESPONSES_ENDPOINT).await;
            assert_eq!(sent_json(&sent[0])["stream"], true);
        }

        #[tokio::test]
        async fn bridged_tool_call_round_trips_through_tool_result() {
            let copilot = copilot().await;
            list_responses_only_model(&copilot);
            Mock::given(method("POST"))
                .and(path(RESPONSES_ENDPOINT))
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
            let sent = requests_to(&copilot, RESPONSES_ENDPOINT).await;
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
                .and(path(RESPONSES_ENDPOINT))
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

        /// An Anthropic body whose latest turn carries one image part.
        fn anthropic_image_body(model: &str) -> Value {
            json!({
                "model": format!("{PROFILE}/{model}"),
                "max_tokens": 64,
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    {"type": "text", "text": "what is this"},
                ]}],
            })
        }

        /// An Anthropic body of about `prompt_tokens` estimated tokens, at one token per four bytes.
        fn long_anthropic_body(model: &str, prompt_tokens: usize) -> Value {
            json!({
                "model": format!("{PROFILE}/{model}"),
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "x".repeat(prompt_tokens * 4)}],
            })
        }

        /// A descriptor for `qualified_id` that reports `vision`.
        fn described(qualified_id: &str, vision: Option<bool>) -> ModelDescriptor {
            ModelDescriptor {
                vision,
                ..ModelDescriptor::for_test(qualified_id)
            }
        }

        /// The model each chat completion sent upstream named, in arrival order.
        async fn chat_models(copilot: &Copilot) -> Vec<String> {
            requests_to(copilot, UPSTREAM_CHAT_PATH)
                .await
                .iter()
                .map(|request| sent_json(request)["model"].as_str().unwrap().to_string())
                .collect()
        }

        /// The 400 message of a request that no model can serve. Panics when the request is served.
        async fn refused_message(
            copilot: &Copilot,
            request_path: &str,
            route: RouteKind,
            headers: &HeaderMap,
            body: &Value,
        ) -> String {
            let Err(error) = attempt_loop(
                &copilot.state,
                Method::POST,
                request_path,
                headers,
                Bytes::from(body.to_string()),
                route,
            )
            .await
            else {
                panic!("the request was served, not refused");
            };
            let AppError::BadRequest(message) = error else {
                panic!("expected a 400, got {error:?}");
            };
            message
        }

        fn fallback_to(model: &str) -> HeaderMap {
            let mut headers = client_headers();
            headers.insert(
                HEADER_FALLBACKS,
                HeaderValue::from_str(&format!("{PROFILE}/{model}")).unwrap(),
            );
            headers
        }

        #[tokio::test]
        async fn image_request_skips_a_text_only_model_and_reaches_a_vision_fallback() {
            let copilot = copilot().await;
            copilot.state.set_model_descriptors(vec![
                described(&format!("{PROFILE}/{PLAIN_ID}"), Some(false)),
                described(&format!("{PROFILE}/{TRANSLATED_ID}"), Some(true)),
            ]);
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &fallback_to(TRANSLATED_ID),
                &anthropic_image_body(PLAIN_ID),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(chat_models(&copilot).await, [TRANSLATED_UPSTREAM_ID]);
        }

        #[tokio::test]
        async fn image_request_to_a_text_only_model_is_refused_without_an_upstream_call() {
            let copilot = copilot().await;
            copilot.state.set_model_descriptors(vec![described(
                &format!("{PROFILE}/{PLAIN_ID}"),
                Some(false),
            )]);

            let message = refused_message(
                &copilot,
                MESSAGES_ENDPOINT,
                RouteKind::Anthropic,
                &client_headers(),
                &anthropic_image_body(PLAIN_ID),
            )
            .await;

            assert_eq!(message, "copilot/gpt-4.1: image input is not supported");
            assert!(
                copilot
                    .upstream
                    .received_requests()
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn a_500_that_refuses_images_teaches_the_model_and_later_image_requests_skip_it() {
            let copilot = copilot().await;
            copilot.state.set_model_descriptors(vec![
                described(&format!("{PROFILE}/{PLAIN_ID}"), None),
                described(&format!("{PROFILE}/{TRANSLATED_ID}"), Some(true)),
            ]);
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .and(body_partial_json(json!({"model": PLAIN_ID})))
                .respond_with(
                    ResponseTemplate::new(500).set_body_string("image input is not supported"),
                )
                .mount(&copilot.upstream)
                .await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .and(body_partial_json(json!({"model": TRANSLATED_UPSTREAM_ID})))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;
            let headers = fallback_to(TRANSLATED_ID);

            let (first, _) = send(
                &copilot,
                &headers,
                &anthropic_image_body(PLAIN_ID),
                RouteKind::Anthropic,
            )
            .await;
            let (second, _) = send(
                &copilot,
                &headers,
                &anthropic_image_body(PLAIN_ID),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!((first, second), (200, 200));
            assert!(copilot.state.vision_unsupported(PROFILE, PLAIN_ID));
            assert_eq!(
                chat_models(&copilot).await,
                [PLAIN_ID, TRANSLATED_UPSTREAM_ID, TRANSLATED_UPSTREAM_ID]
            );
        }

        #[tokio::test]
        async fn fallback_whose_known_limit_is_below_the_prompt_is_skipped() {
            let copilot = copilot().await;
            copilot
                .state
                .model_limits()
                .learn(PROFILE, PLAIN_ID, 1_000, 1);
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &fallback_to(TRANSLATED_ID),
                &long_anthropic_body(PLAIN_ID, 2_000),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(chat_models(&copilot).await, [TRANSLATED_UPSTREAM_ID]);
        }

        #[tokio::test]
        async fn model_with_no_known_limit_is_still_tried_with_a_large_prompt() {
            let copilot = copilot().await;
            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;

            let (status, _) = send(
                &copilot,
                &client_headers(),
                &long_anthropic_body(PLAIN_ID, 2_000),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(chat_models(&copilot).await, [PLAIN_ID]);
        }

        #[tokio::test]
        async fn prompt_over_the_only_known_limit_is_refused_without_an_upstream_call() {
            let copilot = copilot().await;
            copilot
                .state
                .model_limits()
                .learn(PROFILE, PLAIN_ID, 1_000, 1);

            let message = refused_message(
                &copilot,
                MESSAGES_ENDPOINT,
                RouteKind::Anthropic,
                &client_headers(),
                &long_anthropic_body(PLAIN_ID, 2_000),
            )
            .await;

            assert!(
                message.starts_with("copilot/gpt-4.1: a prompt of about "),
                "{message}"
            );
            assert!(
                message.ends_with("is over the known limit of 1000 tokens"),
                "{message}"
            );
            assert!(
                copilot
                    .upstream
                    .received_requests()
                    .await
                    .unwrap()
                    .is_empty()
            );
        }

        #[tokio::test]
        async fn a_rejection_message_teaches_the_limit_for_later_requests() {
            let rejections = [
                json!({"error": {"message": "prompt token count of 2500 exceeds the limit of 1000"}}),
                json!({"error": {"message": "prompt is too long: 2500 tokens > 1000 maximum"}}),
            ];
            for rejection in rejections {
                let copilot = copilot().await;
                Mock::given(method("POST"))
                    .and(path(MESSAGES_ENDPOINT))
                    .respond_with(ResponseTemplate::new(400).set_body_json(rejection))
                    .mount(&copilot.upstream)
                    .await;

                let (first, _) = send(
                    &copilot,
                    &client_headers(),
                    &long_anthropic_body(NATIVE_ID, 500),
                    RouteKind::Anthropic,
                )
                .await;

                assert_eq!(first, 400);
                // The streamed path learns in its stats task, after the body ends.
                for _ in 0..100 {
                    if copilot
                        .state
                        .rejected_prompt_limit(PROFILE, NATIVE_UPSTREAM_ID)
                        .is_some()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    copilot
                        .state
                        .rejected_prompt_limit(PROFILE, NATIVE_UPSTREAM_ID),
                    Some(1_000)
                );

                let message = refused_message(
                    &copilot,
                    MESSAGES_ENDPOINT,
                    RouteKind::Anthropic,
                    &client_headers(),
                    &long_anthropic_body(NATIVE_ID, 2_000),
                )
                .await;

                assert!(
                    message.starts_with("copilot/claude-opus-4-5: a prompt of about "),
                    "{message}"
                );
                assert!(
                    message.ends_with("is over the known limit of 1000 tokens"),
                    "{message}"
                );
                assert_eq!(requests_to(&copilot, MESSAGES_ENDPOINT).await.len(), 1);
            }
        }

        #[tokio::test]
        async fn prompt_estimated_within_the_margin_is_sent_upstream() {
            let copilot = copilot().await;
            copilot
                .state
                .model_limits()
                .learn(PROFILE, PLAIN_ID, 1_000, 1);

            Mock::given(method("POST"))
                .and(path(UPSTREAM_CHAT_PATH))
                .respond_with(chat_completion())
                .mount(&copilot.upstream)
                .await;

            // 1,400 estimated tokens against a 1,000 limit: over the limit, inside the 150% margin.
            let (status, _) = send(
                &copilot,
                &client_headers(),
                &long_anthropic_body(PLAIN_ID, 1_400),
                RouteKind::Anthropic,
            )
            .await;

            assert_eq!(status, 200);
            assert_eq!(chat_models(&copilot).await, [PLAIN_ID]);
        }

        #[tokio::test]
        async fn the_lowest_known_limit_gates_the_request() {
            let copilot = copilot().await;
            copilot
                .state
                .model_limits()
                .learn(PROFILE, NATIVE_UPSTREAM_ID, 500, 1);
            copilot.state.learn_rejected_prompt_limit(
                PROFILE,
                NATIVE_UPSTREAM_ID,
                br#"{"error":{"message":"prompt is too long: 1200 tokens > 1000 maximum"}}"#,
            );

            let message = refused_message(
                &copilot,
                MESSAGES_ENDPOINT,
                RouteKind::Anthropic,
                &client_headers(),
                &long_anthropic_body(NATIVE_ID, 800),
            )
            .await;

            assert!(
                message.ends_with("is over the known limit of 500 tokens"),
                "{message}"
            );
        }

        #[tokio::test]
        async fn copilot_auto_on_a_non_chat_path_is_refused_without_an_upstream_call() {
            let copilot = copilot().await;
            let body = json!({
                "model": format!("{PROFILE}/{COPILOT_AUTO_MODEL}"),
                "messages": [{"role": "user", "content": "hi"}],
            });

            let message = refused_message(
                &copilot,
                "/v1/messages/count_tokens",
                RouteKind::OpenAi,
                &client_headers(),
                &body,
            )
            .await;

            assert_eq!(
                message,
                "copilot/auto routes chat requests only; pick a model for /v1/messages/count_tokens"
            );
            assert!(
                copilot
                    .upstream
                    .received_requests()
                    .await
                    .unwrap()
                    .is_empty()
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

#[cfg(test)]
mod review_body_tests {
    use super::*;

    #[test]
    fn non_streaming_turns_streaming_off_and_drops_stream_options() {
        let body = Bytes::from_static(
            br#"{"model":"m1","stream":true,"stream_options":{"include_usage":true},"messages":[]}"#,
        );
        let value: Value = serde_json::from_slice(&non_streaming(body)).unwrap();
        assert_eq!(value["stream"], false);
        assert!(value.get("stream_options").is_none());
        assert_eq!(value["model"], "m1");
    }

    #[test]
    fn a_body_that_is_not_json_passes_through_unchanged() {
        let body = Bytes::from_static(b"not json");
        assert_eq!(non_streaming(body.clone()), body);
    }

    #[test]
    fn responses_endpoints_match_with_or_without_the_v1_prefix() {
        assert!(is_responses_endpoint("/responses"));
        assert!(is_responses_endpoint("/v1/responses"));
        assert!(!is_responses_endpoint("/v1/chat/completions"));
        assert!(!is_responses_endpoint("/v1/messages"));
    }

    #[test]
    fn review_failed_events_name_the_reason() {
        assert_eq!(review_failed_reason("timeout"), "review_failed: timeout");
    }
}

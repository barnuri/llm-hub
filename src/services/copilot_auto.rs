//! Copilot Auto routing: `POST {api}/auto` picks the model for a conversation,
//! and the server's multi-turn schedule says on which later user turns to ask again.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::schemas::copilot_auto_response::CopilotAutoResponse;
use crate::schemas::effort_level::EffortLevel;
use crate::schemas::model_context;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::turn_signals::TurnSignals;
use crate::services::copilot_auto_decisions::AutoDecisionLog;
use crate::services::model_limits::{ModelLimit, ModelLimits};
use crate::services::request_images::request_has_image;
use crate::services::smart_auto_effort::SmartAutoEffort;
use crate::services::smart_auto_roles::SmartAutoRoles;

const AUTO_PATH: &str = "auto";
/// The bare model id of Copilot Auto on its default tier; `auto-<tier>` names a tier.
pub const AUTO_MODEL: &str = "auto";
const AUTO_TIER_SEPARATOR: char = '-';
use crate::services::copilot_headers::HEADER_API_VERSION;
/// `/auto` rejects every other version with "invalid apiVersion".
const AUTO_API_VERSION: &str = "2026-08-01";
const AUTO_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_REFRESH_MARGIN_SECS: u64 = 5 * 60;
/// A model used this recently has a warm prompt cache, so a scheduled re-route keeps it. This
/// mirrors the 5-minute warm window of the smart-auto affinity cache; the two modules share no symbol.
const CACHE_WARM_SECS: u64 = 5 * 60;
const MAX_CONVERSATIONS: usize = 256;
/// Copilot's chat-completions path, as sent upstream and as `supported_endpoints` lists it.
pub const CHAT_ENDPOINT: &str = "/chat/completions";
/// Copilot's Responses API path, as sent upstream and as `supported_endpoints` lists it.
pub const RESPONSES_ENDPOINT: &str = "/responses";
/// Copilot serves Claude models on the Anthropic Messages API at this path.
pub const MESSAGES_ENDPOINT: &str = "/v1/messages";
const CLAUDE_ID_PREFIX: &str = "claude-";
const TEXT_PART_TYPES: [&str; 2] = ["text", "input_text"];
const TOOL_RESULT_PART_TYPE: &str = "tool_result";
/// The phrase in Auto's `selection_reason`, the complexity level, and the model family.
const COMPLEXITY_FAMILIES: [(&str, &str, &str); 3] = [
    ("low complexity", "low", "haiku"),
    ("medium complexity", "medium", "sonnet"),
    ("high complexity", "high", "opus"),
];
const MAX_SELECTION_REASON_CHARS: usize = 300;
const PROMPT_BYTES_PER_TOKEN: usize = 4;
const ROLE_USER: &str = "user";
const ROLES_SYSTEM: [&str; 2] = ["system", "developer"];

/// A SHA-256 over the length-prefixed parts that identify one conversation.
pub(crate) type ConversationKey = [u8; 32];
type ConversationSlot = tokio::sync::Mutex<Option<Conversation>>;

/// Per-conversation Copilot Auto decisions, shared by every request so a
/// conversation keeps its model between the turns the schedule lets it re-route.
pub struct CopilotAutoRouter {
    conversations: Mutex<ConversationCache>,
    decisions: AutoDecisionLog,
    limits: Arc<ModelLimits>,
}

/// The outcome of routing one `<profile>/auto` request.
#[derive(Clone, PartialEq, Eq)]
pub struct AutoRoute {
    /// The upstream model id the request is sent to.
    pub model: String,
    pub session_token: Option<String>,
    /// Auto's own upstream model id, or the default model when Auto chose nothing.
    pub auto_pick: String,
    /// Why `model` differs from `auto_pick`, when it does.
    pub swap_reason: Option<String>,
    pub complexity: Option<&'static str>,
    pub selection_reason: Option<String>,
    pub turn: AutoTurn,
    pub estimated_prompt_tokens: u64,
    /// The effort to send to `model`, snapped to the levels the catalog publishes for it.
    pub effort: Option<EffortLevel>,
}

/// A Copilot Auto tier, sent as the `tier` field of the `/auto` body.
/// `fast` is the tier VS Code keeps for inline chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoTier {
    Efficiency,
    #[default]
    Balance,
    Intelligence,
    Fast,
}

/// How a turn got its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoTurn {
    /// `/auto` answered on this turn.
    Routed,
    /// A cached conversation decision served the turn.
    Reused,
    /// The router fell back to the default model.
    Default,
}

/// Why the Auto pick cannot serve the request as is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwapReason {
    Endpoint(&'static str),
    Vision,
    PromptTooLarge { estimated: u64, limit: u64 },
}

/// The request facts routing needs.
#[derive(Debug, Clone, Copy)]
pub struct AutoInput<'a> {
    pub tier: AutoTier,
    /// The upstream path, which sets the body shape and the endpoint the model must serve.
    pub path: &'a str,
    pub body: &'a Value,
    /// The raw request size, used to skip models whose prompt window is too small.
    pub prompt_bytes: usize,
    /// The profile's raw `/models` entries.
    pub catalog: &'a [Value],
    /// The model used when `/auto` fails and the conversation has no cached decision.
    pub default_model: &'a str,
}

/// Bounded map of conversation slots, evicted oldest-first.
#[derive(Default)]
struct ConversationCache {
    slots: HashMap<ConversationKey, Arc<ConversationSlot>>,
    order: VecDeque<ConversationKey>,
}

/// Never derive `Debug` here: `session_token` is a secret.
struct Conversation {
    model: ModelTraits,
    session_token: String,
    expires_at: u64,
    anchor_turn: usize,
    checked_turn: usize,
    anchor_scores: HashMap<String, f64>,
    skip_window: u32,
    schedule: Option<Schedule>,
    complexity: Option<&'static str>,
    family_hint: Option<&'static str>,
    selection_reason: Option<String>,
    /// Unix seconds of the last request this conversation served with its model.
    last_used_at: u64,
}

/// The `multi_turn` object of an `/auto` response.
#[derive(Debug, Clone, Deserialize)]
struct Schedule {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    sigma: HashMap<String, f64>,
    escalate_threshold: f64,
    initial_skip: u32,
    backoff_coefficient: u32,
    max_skip: u32,
}

/// The routing-relevant fields of a `/models` entry.
#[derive(Debug, Clone, PartialEq)]
struct ModelTraits {
    id: String,
    endpoints: Option<Vec<String>>,
    price: Option<f64>,
    vision: Option<bool>,
    max_prompt_tokens: Option<u64>,
    in_auto_pool: bool,
    usable: bool,
}

/// The request body format a client path carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyShape {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

/// What one request body says about its conversation and its latest turn.
#[derive(Debug)]
pub(crate) struct Turn {
    pub(crate) key: ConversationKey,
    pub(crate) endpoint: &'static str,
    /// The latest user message text, or `None` for a tool-loop follow-up.
    pub(crate) prompt: Option<String>,
    /// The text of the most recent real user turn, even behind later tool results.
    pub(crate) last_user_text: Option<String>,
    /// Whether any message or tool result in the body holds an image, not only the latest turn.
    pub(crate) has_image: bool,
    pub(crate) user_turns: usize,
}

/// One message-like item of a request body.
pub(crate) struct Item<'a> {
    role: &'a str,
    content: &'a Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    First,
    Check,
}

impl std::fmt::Debug for CopilotAutoRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let conversations = self
            .conversations
            .lock()
            .map_or(0, |cache| cache.slots.len());
        f.debug_struct("CopilotAutoRouter")
            .field("conversations", &conversations)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for AutoRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoRoute")
            .field("model", &self.model)
            .field("has_session_token", &self.session_token.is_some())
            .field("auto_pick", &self.auto_pick)
            .field("swap_reason", &self.swap_reason)
            .field("complexity", &self.complexity)
            .field("selection_reason", &self.selection_reason)
            .field("turn", &self.turn)
            .field("estimated_prompt_tokens", &self.estimated_prompt_tokens)
            .field("effort", &self.effort)
            .finish()
    }
}

impl std::fmt::Display for SwapReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwapReason::Endpoint(endpoint) => write!(f, "does not serve {endpoint}"),
            SwapReason::Vision => f.write_str("does not accept images"),
            SwapReason::PromptTooLarge { estimated, limit } => write!(
                f,
                "allows {limit} prompt tokens, below the estimated {estimated}"
            ),
        }
    }
}

impl AutoTier {
    /// Every tier, in the order the model list shows them.
    pub const ALL: [AutoTier; 4] = [
        AutoTier::Efficiency,
        AutoTier::Balance,
        AutoTier::Intelligence,
        AutoTier::Fast,
    ];

    /// The tier of a bare id: `auto` is the default, `auto-<tier>` names one, and the
    /// retired `eco`, `balanced` and `max` still work. Any other id is not Auto.
    pub fn from_model(bare_id: &str) -> Option<AutoTier> {
        if bare_id == AUTO_MODEL {
            return Some(AutoTier::default());
        }
        let name = bare_id
            .strip_prefix(AUTO_MODEL)?
            .strip_prefix(AUTO_TIER_SEPARATOR)?;
        AutoTier::from_name(name)
    }

    fn from_name(name: &str) -> Option<AutoTier> {
        match name {
            "efficiency" | "eco" => Some(AutoTier::Efficiency),
            "balance" | "balanced" => Some(AutoTier::Balance),
            "intelligence" | "max" => Some(AutoTier::Intelligence),
            "fast" => Some(AutoTier::Fast),
            _ => None,
        }
    }

    /// The wire name `/auto` takes, e.g. `balance`.
    pub fn as_str(self) -> &'static str {
        match self {
            AutoTier::Efficiency => "efficiency",
            AutoTier::Balance => "balance",
            AutoTier::Intelligence => "intelligence",
            AutoTier::Fast => "fast",
        }
    }

    /// The name shown in model pickers, e.g. `Balance`.
    pub fn label(self) -> &'static str {
        match self {
            AutoTier::Efficiency => "Efficiency",
            AutoTier::Balance => "Balance",
            AutoTier::Intelligence => "Intelligence",
            AutoTier::Fast => "Fast",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            AutoTier::Efficiency => {
                "Prioritizes cost. Use for straightforward tasks, quick edits, and simple questions."
            }
            AutoTier::Balance => {
                "Balances cost, quality, and latency. Use for everyday coding and debugging."
            }
            AutoTier::Intelligence => {
                "Prioritizes quality. Use for complex bugs, architecture decisions, and tasks that need deeper reasoning."
            }
            AutoTier::Fast => {
                "Additional hub routing tier. Official Copilot guidance covers Efficiency, Balance, and Intelligence."
            }
        }
    }

    /// The bare model id that selects this tier, e.g. `auto-balance`.
    pub fn model_id(self) -> String {
        format!("{AUTO_MODEL}{AUTO_TIER_SEPARATOR}{}", self.as_str())
    }
}

impl CopilotAutoRouter {
    /// Creates a router with an empty conversation cache and only the seeded prompt limits.
    pub fn new() -> CopilotAutoRouter {
        CopilotAutoRouter::with_limits(Arc::new(ModelLimits::default()))
    }

    /// Creates a router that checks prompt fit against `limits`.
    pub fn with_limits(limits: Arc<ModelLimits>) -> CopilotAutoRouter {
        CopilotAutoRouter {
            conversations: Mutex::new(ConversationCache::default()),
            decisions: AutoDecisionLog::default(),
            limits,
        }
    }

    /// The log the proxy records each routed request into.
    pub fn decisions(&self) -> &AutoDecisionLog {
        &self.decisions
    }

    /// Routes `input` on its tier and returns the upstream model id, its session token, what Auto
    /// decided and the effort to send to that model. Each tier keeps its own conversation decision.
    pub async fn route(&self, upstream: &AutoUpstream<'_>, input: &AutoInput<'_>) -> AutoRoute {
        let mut route = self.route_at(upstream, input, unix_now_secs()).await;
        route.effort = pick_effort(input, &route.model);
        route
    }

    async fn route_at(
        &self,
        upstream: &AutoUpstream<'_>,
        input: &AutoInput<'_>,
        now_secs: u64,
    ) -> AutoRoute {
        let estimated = estimated_prompt_tokens(input.prompt_bytes);
        let Some(turn) = Turn::parse(upstream.profile, input.tier, input.path, input.body) else {
            return AutoRoute::default_model(input.default_model, estimated);
        };
        let slot = self.slot(turn.key);
        let mut state = slot.lock().await;
        let kind = match (state.as_ref(), turn.prompt.as_deref()) {
            (None, Some(_)) => Some(RouteKind::First),
            (Some(conversation), Some(_)) => conversation.route_kind(turn.user_turns, now_secs),
            (_, None) => None,
        };
        let mut routed = false;
        if let (Some(kind), Some(prompt)) = (kind, turn.prompt.as_deref()) {
            let request = auto_request_body(
                prompt,
                input.tier,
                &turn,
                state.as_ref().filter(|_| kind == RouteKind::Check),
            );
            match fetch_decision(upstream, &request).await {
                Ok(decision) => {
                    routed = apply_decision(
                        &mut state,
                        kind,
                        decision,
                        turn.user_turns,
                        upstream.profile,
                        now_secs,
                    );
                }
                Err(e) => tracing::warn!(
                    "profile {}: Copilot Auto routing failed, so the request uses the cached or default model: {e}",
                    upstream.profile
                ),
            }
        }
        let Some(conversation) = state.as_mut() else {
            return AutoRoute::default_model(input.default_model, estimated);
        };
        conversation.last_used_at = now_secs;
        let pick = &conversation.model.id;
        let limits = self.limits.for_profile(upstream.profile);
        let Some((model, swap)) = compatible_model(
            &conversation.model,
            turn.endpoint,
            turn.has_image,
            input.catalog,
            conversation.family_hint,
            Fit {
                estimated_tokens: estimated,
                limits: &limits,
            },
        ) else {
            tracing::warn!(
                "profile {}: no Copilot model serves {} for the Auto pick {pick}, so the request uses the default model",
                upstream.profile,
                turn.endpoint,
            );
            return AutoRoute {
                auto_pick: pick.clone(),
                complexity: conversation.complexity,
                selection_reason: conversation.selection_reason.clone(),
                ..AutoRoute::default_model(input.default_model, estimated)
            };
        };
        let token =
            (now_secs < conversation.expires_at).then(|| conversation.session_token.clone());
        AutoRoute {
            model,
            session_token: token,
            auto_pick: pick.clone(),
            swap_reason: swap.map(|reason| format!("Auto pick {pick} {reason}")),
            complexity: conversation.complexity,
            selection_reason: conversation.selection_reason.clone(),
            turn: if routed {
                AutoTurn::Routed
            } else {
                AutoTurn::Reused
            },
            estimated_prompt_tokens: estimated,
            effort: None,
        }
    }

    fn slot(&self, key: ConversationKey) -> Arc<ConversationSlot> {
        let mut cache = self
            .conversations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = cache.slots.get(&key) {
            return Arc::clone(slot);
        }
        while cache.slots.len() >= MAX_CONVERSATIONS {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.slots.remove(&oldest);
        }
        let slot = Arc::new(tokio::sync::Mutex::new(None));
        cache.slots.insert(key, Arc::clone(&slot));
        cache.order.push_back(key);
        slot
    }
}

impl Default for CopilotAutoRouter {
    fn default() -> CopilotAutoRouter {
        CopilotAutoRouter::new()
    }
}

impl AutoRoute {
    fn default_model(default_model: &str, estimated_prompt_tokens: u64) -> AutoRoute {
        AutoRoute {
            model: default_model.to_string(),
            session_token: None,
            auto_pick: default_model.to_string(),
            swap_reason: None,
            complexity: None,
            selection_reason: None,
            turn: AutoTurn::Default,
            estimated_prompt_tokens,
            effort: None,
        }
    }
}

/// The effort to send to `model`: the caller's explicit level when the model publishes it, else
/// the tier and role objective. Either snaps to the levels the catalog publishes for `model`.
fn pick_effort(input: &AutoInput<'_>, model: &str) -> Option<EffortLevel> {
    let published: Vec<String> = input
        .catalog
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?
        .pointer("/capabilities/supports/reasoning_effort")?
        .as_array()?
        .iter()
        .filter_map(|level| level.as_str().map(str::to_owned))
        .collect();
    if published.is_empty() {
        return None;
    }
    let requested = SmartAutoEffort::requested(bare_path(input.path), input.body)
        .ok()
        .flatten();
    let signals = TurnSignals::from_body(input.path, input.body, input.prompt_bytes, None);
    let turn = SmartAutoRoles::classify_user_text(&signals, None);
    SmartAutoEffort::select(
        SmartAutoTier::from(input.tier),
        turn.role,
        turn.plan_backed,
        requested,
        &published,
    )
}

/// Roughly one token per four request bytes: too rough for anything but ruling
/// out prompt windows that are clearly too small.
pub(crate) fn estimated_prompt_tokens(prompt_bytes: usize) -> u64 {
    u64::try_from(prompt_bytes / PROMPT_BYTES_PER_TOKEN).unwrap_or(u64::MAX)
}

/// Where and as whom `/auto` is called.
pub struct AutoUpstream<'a> {
    pub client: &'a reqwest::Client,
    pub profile: &'a str,
    pub bearer: &'a str,
    pub api_base: &'a str,
}

impl std::fmt::Debug for AutoUpstream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoUpstream")
            .field("profile", &self.profile)
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

/// The request path without its query string.
pub fn bare_path(path: &str) -> &str {
    path.split_once('?').map_or(path, |(bare, _)| bare)
}

impl Conversation {
    fn anchored(
        decision: CopilotAutoResponse,
        model: ModelTraits,
        turn: usize,
        now_secs: u64,
    ) -> Conversation {
        let schedule = parse_schedule(decision.multi_turn.as_ref());
        let complexity = complexity(decision.selection_reason.as_deref());
        Conversation {
            model,
            session_token: decision.session_token,
            expires_at: decision.expires_at,
            anchor_turn: turn,
            checked_turn: turn,
            anchor_scores: decision.hydra_scores,
            skip_window: schedule.as_ref().map_or(0, |s| s.initial_skip),
            schedule,
            complexity: complexity.map(|(level, _)| level),
            family_hint: complexity.map(|(_, family)| family),
            selection_reason: truncated_reason(decision.selection_reason),
            last_used_at: now_secs,
        }
    }

    /// Whether this user turn routes: a fresh route once the session token is
    /// near expiry, a schedule check once the skip window has passed.
    fn route_kind(&self, user_turns: usize, now_secs: u64) -> Option<RouteKind> {
        if now_secs.saturating_add(SESSION_REFRESH_MARGIN_SECS) >= self.expires_at {
            return Some(RouteKind::First);
        }
        if !self.schedule.as_ref().is_some_and(|s| s.enabled) {
            return None;
        }
        let skip_window = usize::try_from(self.skip_window).unwrap_or(usize::MAX);
        let turns_since_check = user_turns.checked_sub(self.checked_turn)?;
        (turns_since_check > skip_window).then_some(RouteKind::Check)
    }

    /// Whether the task-type scores moved from the anchor by the schedule's threshold, in
    /// sigmas. Without comparable scores, a different model counts as escalation.
    fn escalates(&self, decision: &CopilotAutoResponse, model: &ModelTraits) -> bool {
        let Some(schedule) = self.schedule.as_ref() else {
            return model.id != self.model.id;
        };
        let drifts: Vec<f64> = schedule
            .sigma
            .iter()
            .filter(|(_, sigma)| **sigma > 0.0)
            .filter_map(|(task, sigma)| {
                let anchor = self.anchor_scores.get(task)?;
                let current = decision.hydra_scores.get(task)?;
                Some((current - anchor).abs() / sigma)
            })
            .collect();
        if drifts.is_empty() {
            return model.id != self.model.id;
        }
        drifts
            .into_iter()
            .any(|drift| drift >= schedule.escalate_threshold)
    }

    /// Whether a scheduled re-route to `model` keeps the current model. A different model would
    /// rebuild a warm prompt cache, and the switch is optional, so it waits until the conversation
    /// has been idle for `CACHE_WARM_SECS`.
    fn keeps_warm_model(&self, model: &ModelTraits, now_secs: u64) -> bool {
        model.id != self.model.id && now_secs < self.last_used_at.saturating_add(CACHE_WARM_SECS)
    }

    /// Keeps the anchored model after a check that did not escalate, takes the
    /// fresh session token and widens the skip window.
    fn back_off(&mut self, decision: CopilotAutoResponse, turn: usize) {
        if let Some(schedule) = parse_schedule(decision.multi_turn.as_ref()) {
            self.schedule = Some(schedule);
        }
        if let Some(schedule) = self.schedule.as_ref() {
            self.skip_window = self
                .skip_window
                .max(1)
                .saturating_mul(schedule.backoff_coefficient)
                .min(schedule.max_skip);
        }
        let complexity = complexity(decision.selection_reason.as_deref());
        self.complexity = complexity.map(|(level, _)| level);
        self.family_hint = complexity.map(|(_, family)| family);
        self.selection_reason = truncated_reason(decision.selection_reason);
        self.session_token = decision.session_token;
        self.expires_at = decision.expires_at;
        self.checked_turn = turn;
    }
}

/// Applies an `/auto` answer to the conversation; false when it named no model.
fn apply_decision(
    state: &mut Option<Conversation>,
    kind: RouteKind,
    decision: CopilotAutoResponse,
    turn: usize,
    profile: &str,
    now_secs: u64,
) -> bool {
    let Some(model) = ModelTraits::from_entry(&decision.selected_model) else {
        tracing::warn!("profile {profile}: the Copilot Auto response named no selected model");
        return false;
    };
    tracing::info!(
        "profile {profile}: Copilot Auto selected {} (discount {}): {}",
        model.id,
        decision
            .discounted_costs
            .get(&model.id)
            .copied()
            .unwrap_or(0.0),
        decision
            .selection_reason
            .as_deref()
            .unwrap_or("no reason given")
    );
    if let Some(conversation) = state.as_mut().filter(|_| kind == RouteKind::Check)
        && (!conversation.escalates(&decision, &model)
            || conversation.keeps_warm_model(&model, now_secs))
    {
        conversation.back_off(decision, turn);
        return true;
    }
    *state = Some(Conversation::anchored(decision, model, turn, now_secs));
    true
}

fn parse_schedule(multi_turn: Option<&Value>) -> Option<Schedule> {
    multi_turn.and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// The `/auto` body: the prompt, the tier, the image flag, and on a schedule
/// check the conversation's multi-turn state.
fn auto_request_body(
    prompt: &str,
    tier: AutoTier,
    turn: &Turn,
    conversation: Option<&Conversation>,
) -> Value {
    let mut body = Map::new();
    body.insert("prompt".to_string(), Value::from(prompt));
    body.insert("tier".to_string(), Value::from(tier.as_str()));
    if turn.has_image {
        body.insert("has_image".to_string(), Value::Bool(true));
    }
    if let Some(conversation) = conversation {
        body.insert(
            "multi_turn".to_string(),
            json!({
                "turns_since_anchor": turn.user_turns.saturating_sub(conversation.anchor_turn),
                "current_skip_window": conversation.skip_window,
                "anchor_cap_vector": conversation.anchor_scores,
            }),
        );
    }
    Value::Object(body)
}

async fn fetch_decision(
    upstream: &AutoUpstream<'_>,
    request: &Value,
) -> Result<CopilotAutoResponse, String> {
    let url = format!("{}/{AUTO_PATH}", upstream.api_base.trim_end_matches('/'));
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", upstream.bearer))
        .map_err(|_| "the Copilot bearer is not a valid header value".to_string())?;
    bearer.set_sensitive(true);
    let started = Instant::now();
    let response = upstream
        .client
        .post(url)
        .timeout(AUTO_TIMEOUT)
        .header(AUTHORIZATION, bearer)
        .header(HEADER_API_VERSION, AUTO_API_VERSION)
        .json(request)
        .send()
        .await
        .map_err(|e| format!("POST /auto did not complete: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("POST /auto returned status {status}"));
    }
    let decision = response
        .json::<CopilotAutoResponse>()
        .await
        .map_err(|e| format!("POST /auto returned an unreadable body: {e}"))?;
    tracing::info!(
        "profile {}: POST /auto took {} ms",
        upstream.profile,
        started.elapsed().as_millis()
    );
    Ok(decision)
}

/// The model that serves `endpoint` and the image and fits the prompt (an unknown limit fits).
/// A swap goes by closest price, else Auto's complexity family; if none fits, the largest window.
fn compatible_model(
    selected: &ModelTraits,
    endpoint: &'static str,
    has_image: bool,
    catalog: &[Value],
    family_hint: Option<&str>,
    fit: Fit<'_>,
) -> Option<(String, Option<SwapReason>)> {
    let estimated_tokens = fit.estimated_tokens;
    let models: Vec<ModelTraits> = catalog.iter().filter_map(ModelTraits::from_entry).collect();
    let listed = models.iter().find(|model| model.id == selected.id);
    let endpoints = selected
        .endpoints
        .as_ref()
        .or_else(|| listed.and_then(|model| model.endpoints.as_ref()));
    let serves_endpoint = endpoints.is_none_or(|list| serves(list, endpoint));
    let serves_image = !has_image || selected.vision != Some(false);
    let pick_listed = selected
        .max_prompt_tokens
        .or_else(|| listed.and_then(|model| model.max_prompt_tokens));
    let pick_limit = fit.limit(&selected.id, pick_listed, endpoint);
    let pick_too_small = pick_limit.filter(|limit| *limit < estimated_tokens);
    let reason = match (serves_endpoint, serves_image, pick_too_small) {
        (false, _, _) => SwapReason::Endpoint(endpoint),
        (true, false, _) => SwapReason::Vision,
        (true, true, Some(limit)) => SwapReason::PromptTooLarge {
            estimated: estimated_tokens,
            limit,
        },
        (true, true, None) => return Some((selected.id.clone(), None)),
    };
    let compatible: Vec<&ModelTraits> = models
        .iter()
        .filter(|model| model.usable && model.id != selected.id)
        .filter(|model| model.supports(endpoint))
        .filter(|model| !has_image || model.vision == Some(true))
        .collect();
    let candidates: Vec<&ModelTraits> = compatible
        .iter()
        .copied()
        .filter(|model| fit.fits(model, endpoint))
        .collect();
    if candidates.is_empty() {
        let pick_competes = matches!(reason, SwapReason::PromptTooLarge { .. });
        let window = |model: &&ModelTraits| {
            if model.id == selected.id {
                pick_limit
            } else {
                fit.limit(&model.id, model.max_prompt_tokens, endpoint)
            }
        };
        let largest = compatible
            .into_iter()
            .chain(pick_competes.then_some(selected))
            .max_by_key(window)?;
        let swap = (largest.id != selected.id).then_some(reason);
        return Some((largest.id.clone(), swap));
    }
    let pool: Vec<&ModelTraits> = if candidates.iter().any(|model| model.in_auto_pool) {
        candidates
            .into_iter()
            .filter(|model| model.in_auto_pool)
            .collect()
    } else {
        candidates
    };
    let target = selected
        .price
        .or_else(|| listed.and_then(|model| model.price));
    let swapped = match target.filter(|_| pool.iter().any(|model| model.price.is_some())) {
        Some(target) => pool
            .into_iter()
            .min_by(|a, b| {
                price_distance(a.price, target).total_cmp(&price_distance(b.price, target))
            })
            .map(|model| model.id.clone()),
        None => newest_in_family(&pool, family_hint)
            .or_else(|| pool.first().map(|model| model.id.clone())),
    };
    swapped.map(|model| (model, Some(reason)))
}

/// Auto's `selection_reason` names the task complexity; map it to a level and a model family.
fn complexity(reason: Option<&str>) -> Option<(&'static str, &'static str)> {
    let reason = reason?.to_ascii_lowercase();
    COMPLEXITY_FAMILIES
        .iter()
        .find(|(phrase, _, _)| reason.contains(phrase))
        .map(|(_, level, family)| (*level, *family))
}

fn truncated_reason(mut reason: Option<String>) -> Option<String> {
    if let Some(text) = reason.as_mut()
        && let Some((cut, _)) = text.char_indices().nth(MAX_SELECTION_REASON_CHARS)
    {
        text.truncate(cut);
    }
    reason
}

/// The highest-versioned pool model whose id contains `family`.
fn newest_in_family(pool: &[&ModelTraits], family: Option<&str>) -> Option<String> {
    let family = family?;
    pool.iter()
        .filter(|model| model.id.contains(family))
        .max_by_key(|model| version_key(&model.id))
        .map(|model| model.id.clone())
}

fn version_key(id: &str) -> Vec<u32> {
    id.split(|c: char| !c.is_ascii_digit())
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Distance on a log scale, so price tiers compare by ratio; an unpriced model is farthest.
fn price_distance(price: Option<f64>, target: f64) -> f64 {
    price.map_or(f64::INFINITY, |price| {
        ((price + 1.0) / (target + 1.0)).ln().abs()
    })
}

impl ModelTraits {
    fn from_entry(entry: &Value) -> Option<ModelTraits> {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?;
        let endpoints = entry
            .get("supported_endpoints")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
        let billing = entry.get("billing");
        let usable = entry.get("model_picker_enabled").and_then(Value::as_bool) != Some(false)
            && entry.pointer("/policy/state").and_then(Value::as_str) != Some("disabled")
            && entry
                .pointer("/capabilities/type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "chat");
        Some(ModelTraits {
            id: id.to_string(),
            endpoints,
            price: billing.and_then(billing_price),
            vision: entry
                .pointer("/capabilities/supports/vision")
                .and_then(Value::as_bool),
            max_prompt_tokens: model_context::max_input_tokens_from_item(entry),
            in_auto_pool: billing.and_then(|b| b.get("auto_discount")).is_some(),
            usable,
        })
    }

    fn supports(&self, endpoint: &str) -> bool {
        self.endpoints
            .as_ref()
            .is_some_and(|list| serves(list, endpoint))
    }
}

/// True when `endpoints` lists `endpoint`. The proxy bridges an Anthropic
/// request to `/responses`, so a Responses model also serves `/v1/messages`.
fn serves(endpoints: &[String], endpoint: &str) -> bool {
    endpoints.iter().any(|listed| {
        listed == endpoint || (endpoint == MESSAGES_ENDPOINT && listed == RESPONSES_ENDPOINT)
    })
}

/// The estimated prompt size and the profile's learned or seeded prompt limits.
#[derive(Clone, Copy)]
struct Fit<'a> {
    estimated_tokens: u64,
    limits: &'a HashMap<String, ModelLimit>,
}

impl Fit<'_> {
    /// The learned or seeded limit for `id`, else the `/models` one. On `/v1/messages`
    /// Copilot accepts far more than it lists for Claude, so that listing never counts.
    fn limit(&self, id: &str, listed: Option<u64>, endpoint: &str) -> Option<u64> {
        if let Some(known) = self.limits.get(id) {
            return Some(known.limit);
        }
        let listed_is_floor = endpoint == MESSAGES_ENDPOINT && id.starts_with(CLAUDE_ID_PREFIX);
        listed.filter(|_| !listed_is_floor)
    }

    fn fits(&self, model: &ModelTraits, endpoint: &str) -> bool {
        self.limit(&model.id, model.max_prompt_tokens, endpoint)
            .is_none_or(|limit| limit >= self.estimated_tokens)
    }
}

/// The default-context input plus output token price, else the premium-request multiplier.
fn billing_price(billing: &Value) -> Option<f64> {
    let prices = billing.pointer("/token_prices/default");
    let input = prices
        .and_then(|p| p.get("input_price"))
        .and_then(Value::as_f64);
    let output = prices
        .and_then(|p| p.get("output_price"))
        .and_then(Value::as_f64);
    match (input, output) {
        (Some(input), Some(output)) => Some(input + output),
        _ => billing.get("multiplier").and_then(Value::as_f64),
    }
}

impl BodyShape {
    fn from_path(path: &str) -> Option<BodyShape> {
        let path = bare_path(path).trim_end_matches('/');
        if path.ends_with("chat/completions") {
            Some(BodyShape::ChatCompletions)
        } else if path.ends_with("responses") {
            Some(BodyShape::Responses)
        } else if path.ends_with("messages") {
            Some(BodyShape::AnthropicMessages)
        } else {
            None
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            BodyShape::ChatCompletions => CHAT_ENDPOINT,
            BodyShape::Responses => RESPONSES_ENDPOINT,
            BodyShape::AnthropicMessages => MESSAGES_ENDPOINT,
        }
    }
}

impl Turn {
    /// Reads the turn from a body whose shape `path` names; `None` for any other path.
    /// The key hashes `profile`, `tier`, the system prompt and the first user message.
    pub(crate) fn parse(profile: &str, tier: AutoTier, path: &str, body: &Value) -> Option<Turn> {
        let shape = BodyShape::from_path(path)?;
        let items = items(shape, body);
        let system = system_text(shape, body, &items);
        let mut user_texts: Vec<String> = items.iter().filter_map(Item::user_turn_text).collect();
        let latest = items
            .iter()
            .rev()
            .find(|item| !ROLES_SYSTEM.contains(&item.role));
        let prompt = latest.and_then(Item::user_turn_text);
        let has_image = request_has_image(bare_path(path), body);
        let first_user = user_texts.first().map_or("", String::as_str);
        let key = conversation_key(profile, tier, &system, first_user);
        let user_turns = user_texts.len();
        Some(Turn {
            key,
            endpoint: shape.endpoint(),
            prompt,
            last_user_text: user_texts.pop(),
            has_image,
            user_turns,
        })
    }
}

/// The message-like items of a body: `input` for Responses, else `messages`.
pub(crate) fn items(shape: BodyShape, body: &Value) -> Vec<Item<'_>> {
    let key = match shape {
        BodyShape::Responses => "input",
        BodyShape::ChatCompletions | BodyShape::AnthropicMessages => "messages",
    };
    match body.get(key) {
        Some(text @ Value::String(_)) => vec![Item {
            role: ROLE_USER,
            content: text,
        }],
        Some(Value::Array(list)) => list
            .iter()
            .map(|item| Item::from_value(shape, item))
            .collect(),
        _ => Vec::new(),
    }
}

impl<'a> Item<'a> {
    /// A Responses item that is not a message, such as `function_call_output`,
    /// gets an empty role so it never counts as a user turn.
    fn from_value(shape: BodyShape, item: &'a Value) -> Item<'a> {
        let is_message = shape != BodyShape::Responses
            || item
                .get("type")
                .and_then(Value::as_str)
                .is_none_or(|kind| kind == "message");
        let role = item
            .get("role")
            .and_then(Value::as_str)
            .filter(|_| is_message);
        Item {
            role: role.unwrap_or(""),
            content: item.get("content").unwrap_or(&Value::Null),
        }
    }

    /// The text of a real user turn. A user message that carries tool results
    /// is a tool-loop follow-up, even when it also holds text such as a reminder.
    fn user_turn_text(&self) -> Option<String> {
        if self.role != ROLE_USER || content_has_tool_result(self.content) {
            return None;
        }
        Some(content_text(self.content)).filter(|text| !text.trim().is_empty())
    }
}

fn content_has_tool_result(content: &Value) -> bool {
    let Value::Array(parts) = content else {
        return false;
    };
    parts
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some(TOOL_RESULT_PART_TYPE))
}

/// The top-level system prompt, else the first system or developer message's text.
pub(crate) fn system_text(shape: BodyShape, body: &Value, items: &[Item<'_>]) -> String {
    let top_level = match shape {
        BodyShape::AnthropicMessages => body.get("system"),
        BodyShape::Responses => body.get("instructions"),
        BodyShape::ChatCompletions => None,
    };
    if let Some(system) = top_level {
        return content_text(system);
    }
    items
        .iter()
        .find(|item| ROLES_SYSTEM.contains(&item.role))
        .map(|item| content_text(item.content))
        .unwrap_or_default()
}

/// The text of a string content or of its text parts; tool results and images add nothing.
fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| {
                part.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| TEXT_PART_TYPES.contains(&kind))
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Hashes the profile, tier, system prompt and first user message, each length-prefixed,
/// so the key is stable across a conversation's turns and a tier switch starts afresh.
pub(crate) fn conversation_key(
    profile: &str,
    tier: AutoTier,
    system: &str,
    first_user: &str,
) -> ConversationKey {
    let mut hasher = Sha256::new();
    for part in [profile, tier.as_str(), system, first_user] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().into()
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::io;

    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::services::model_limits::LimitSource;

    const PROFILE: &str = "copilot";
    const BEARER: &str = "gho_from_gh";
    const DEFAULT_MODEL: &str = "gpt-default";
    const NOW: u64 = 1_800_000_000;
    const DAY_SECS: u64 = 24 * 60 * 60;
    const UPSTREAM_CHAT_PATH: &str = "/chat/completions";
    const UPSTREAM_MESSAGES_PATH: &str = "/v1/messages";

    fn auto_response(
        model: &Value,
        token: &str,
        expires_at: u64,
        scores: &Value,
    ) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "session_token": token,
            "expires_at": expires_at,
            "selected_model": model,
            "selection_reason": "test pick",
            "hydra_scores": scores,
            "multi_turn": {
                "enabled": true,
                "sigma": {"code_gen": 0.2, "reasoning": 0.2},
                "escalate_threshold": 2,
                "initial_skip": 2,
                "backoff_coefficient": 2,
                "max_skip": 32,
            },
            "discounted_costs": {},
        }))
    }

    fn chat_model(id: &str) -> Value {
        json!({"id": id, "supported_endpoints": ["/chat/completions"]})
    }

    fn scores(code_gen: f64) -> Value {
        json!({"code_gen": code_gen, "reasoning": 0.3})
    }

    /// A chat body whose user turns are `prompts`, all sharing one system prompt.
    fn chat_body(prompts: &[&str]) -> Value {
        let mut messages = vec![json!({"role": "system", "content": "be terse"})];
        for prompt in prompts {
            messages.push(json!({"role": "user", "content": prompt}));
            messages.push(json!({"role": "assistant", "content": "ok"}));
        }
        messages.pop();
        json!({"model": "copilot/auto", "messages": messages})
    }

    fn tool_follow_up(prompts: &[&str]) -> Value {
        let mut body = chat_body(prompts);
        let messages = body["messages"].as_array_mut().unwrap();
        messages.push(json!({"role": "assistant", "tool_calls": [{"id": "c1"}]}));
        messages.push(json!({"role": "tool", "tool_call_id": "c1", "content": "42"}));
        body
    }

    async fn route(
        router: &CopilotAutoRouter,
        server: &MockServer,
        upstream_path: &str,
        body: &Value,
        catalog: &[Value],
        now: u64,
    ) -> (String, Option<String>) {
        let route = route_sized(router, server, upstream_path, body, 0, catalog, now).await;
        (route.model, route.session_token)
    }

    async fn route_sized(
        router: &CopilotAutoRouter,
        server: &MockServer,
        upstream_path: &str,
        body: &Value,
        prompt_bytes: usize,
        catalog: &[Value],
        now: u64,
    ) -> AutoRoute {
        let client = reqwest::Client::new();
        let uri = server.uri();
        let upstream = AutoUpstream {
            client: &client,
            profile: PROFILE,
            bearer: BEARER,
            api_base: &uri,
        };
        let input = AutoInput {
            tier: AutoTier::Balance,
            path: upstream_path,
            body,
            prompt_bytes,
            catalog,
            default_model: DEFAULT_MODEL,
        };
        router.route_at(&upstream, &input, now).await
    }

    fn auto_bodies(requests: &[Request]) -> Vec<Value> {
        requests
            .iter()
            .filter(|request| request.url.path() == "/auto")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn first_turn_calls_auto_and_tool_follow_up_reuses_the_decision() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(header("authorization", format!("Bearer {BEARER}").as_str()))
            .and(header("x-github-api-version", AUTO_API_VERSION))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(json!({"prompt": "fix the bug"})))
            .respond_with(auto_response(
                &chat_model("gpt-picked"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();

        let first = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["fix the bug"]),
            &[],
            NOW,
        )
        .await;
        let follow_up = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &tool_follow_up(&["fix the bug"]),
            &[],
            NOW,
        )
        .await;

        let expected = ("gpt-picked".to_string(), Some("sess-1".to_string()));
        assert_eq!(first, expected);
        assert_eq!(follow_up, expected);
        let bodies = auto_bodies(&server.received_requests().await.unwrap());
        assert_eq!(
            bodies[0],
            json!({"prompt": "fix the bug", "tier": "balance"})
        );
    }

    #[tokio::test]
    async fn concurrent_first_turns_share_one_auto_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-picked"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let body = chat_body(&["hello"]);

        let (a, b) = tokio::join!(
            route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], NOW),
            route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], NOW),
        );

        assert_eq!(a, b);
        assert_eq!(a.0, "gpt-picked");
    }

    #[tokio::test]
    async fn schedule_re_routes_after_the_skip_window_and_escalates_on_drift() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.1),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-big"),
                "sess-2",
                NOW + DAY_SECS,
                &scores(0.9),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let turns = ["one", "two", "three", "four"];

        let mut picked = Vec::new();
        for count in 1..=turns.len() {
            let body = chat_body(&turns[..count]);
            // The drift check runs on a cold cache, so its escalation is not held back as warm.
            let now = if count == turns.len() {
                NOW + CACHE_WARM_SECS
            } else {
                NOW
            };
            picked.push(
                route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], now)
                    .await
                    .0,
            );
        }

        assert_eq!(picked, ["gpt-small", "gpt-small", "gpt-small", "gpt-big"]);
        let bodies = auto_bodies(&server.received_requests().await.unwrap());
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["prompt"], "four");
        assert_eq!(bodies[1]["multi_turn"]["turns_since_anchor"], 3);
        assert_eq!(bodies[1]["multi_turn"]["current_skip_window"], 2);
        assert_eq!(bodies[1]["multi_turn"]["anchor_cap_vector"], scores(0.1));
    }

    #[tokio::test]
    async fn schedule_check_without_drift_keeps_the_model_and_doubles_the_window() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-other"),
                "sess-2",
                NOW + DAY_SECS,
                &scores(0.55),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let turns = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

        let mut picked = Vec::new();
        for count in 1..=turns.len() {
            let body = chat_body(&turns[..count]);
            picked.push(route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], NOW).await);
        }

        assert!(picked.iter().all(|(model, _)| model == "gpt-small"));
        assert_eq!(picked[3].1.as_deref(), Some("sess-2"));
        let bodies = auto_bodies(&server.received_requests().await.unwrap());
        let prompts: Vec<&str> = bodies
            .iter()
            .map(|b| b["prompt"].as_str().unwrap())
            .collect();
        assert_eq!(prompts, ["1", "4", "9"]);
        assert_eq!(bodies[2]["multi_turn"]["current_skip_window"], 4);
    }

    #[tokio::test]
    async fn repeated_user_turn_does_not_re_route() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let body = chat_body(&["same"]);

        for _ in 0..5 {
            route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], NOW).await;
        }
    }

    #[tokio::test]
    async fn expiring_session_token_is_refreshed_on_the_next_user_turn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-old",
                NOW + 60,
                &scores(0.5),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-new",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();

        let first = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a"]),
            &[],
            NOW,
        )
        .await;
        let tool = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &tool_follow_up(&["a"]),
            &[],
            NOW + 120,
        )
        .await;
        let second = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a", "b"]),
            &[],
            NOW,
        )
        .await;

        assert_eq!(first.1.as_deref(), Some("sess-old"));
        assert_eq!(tool, ("gpt-small".to_string(), None));
        assert_eq!(second.1.as_deref(), Some("sess-new"));
        assert_eq!(
            auto_bodies(&server.received_requests().await.unwrap()).len(),
            2
        );
    }

    /// Auto anchors `gpt-small` on the prompt "one" and picks a drifted `gpt-big` on "four".
    async fn anchor_then_drift(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(body_partial_json(json!({"prompt": "one"})))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.1),
            ))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(body_partial_json(json!({"prompt": "four"})))
            .respond_with(auto_response(
                &chat_model("gpt-big"),
                "sess-2",
                NOW + DAY_SECS,
                &scores(0.9),
            ))
            .mount(server)
            .await;
    }

    /// The four-turn body whose check picks `gpt-big`: its skip window of 2 has passed.
    fn check_body() -> Value {
        chat_body(&["one", "two", "three", "four"])
    }

    #[tokio::test]
    async fn warm_scheduled_reroute_keeps_the_model() {
        let server = MockServer::start().await;
        anchor_then_drift(&server).await;
        let router = CopilotAutoRouter::new();
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &[],
            NOW,
        )
        .await;

        let (picked, _) = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &check_body(),
            &[],
            NOW + 60,
        )
        .await;

        assert_eq!(picked, "gpt-small");
        assert_eq!(
            auto_bodies(&server.received_requests().await.unwrap()).len(),
            2
        );
    }

    #[tokio::test]
    async fn cold_scheduled_reroute_switches_to_the_new_model() {
        let server = MockServer::start().await;
        anchor_then_drift(&server).await;
        let router = CopilotAutoRouter::new();
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &[],
            NOW,
        )
        .await;

        let (picked, _) = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &check_body(),
            &[],
            NOW + CACHE_WARM_SECS,
        )
        .await;

        assert_eq!(picked, "gpt-big");
    }

    #[tokio::test]
    async fn tool_follow_up_keeps_the_conversation_warm() {
        let server = MockServer::start().await;
        anchor_then_drift(&server).await;
        let router = CopilotAutoRouter::new();
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &[],
            NOW,
        )
        .await;
        // A tool-loop follow-up never calls /auto, but it does use the model.
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &tool_follow_up(&["one", "two", "three"]),
            &[],
            NOW + 200,
        )
        .await;

        let (picked, _) = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &check_body(),
            &[],
            NOW + 400,
        )
        .await;

        assert_eq!(picked, "gpt-small");
    }

    #[tokio::test]
    async fn image_escalation_still_switches_while_warm() {
        let server = MockServer::start().await;
        let mut blind = priced("gpt-small", &[CHAT_ENDPOINT], 1.0, 1.0);
        blind["capabilities"]["supports"] = json!({"vision": false});
        let mut seeing = priced("gpt-vision", &[CHAT_ENDPOINT], 1.0, 1.0);
        seeing["capabilities"]["supports"] = json!({"vision": true});
        let catalog = [blind.clone(), seeing];
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &blind,
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.1),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &catalog,
            NOW,
        )
        .await;
        let mut image_body = chat_body(&["one"]);
        image_body["messages"][1]["content"] = json!([
            {"type": "text", "text": "one"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]);

        let image_route = route_sized(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &image_body,
            0,
            &catalog,
            NOW + 60,
        )
        .await;
        let (text_model, _) = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &catalog,
            NOW + 120,
        )
        .await;

        assert_eq!(image_route.model, "gpt-vision");
        assert!(image_route.swap_reason.is_some());
        assert_eq!(text_model, "gpt-small");
    }

    #[tokio::test]
    async fn prompt_over_the_window_still_switches_while_warm() {
        let server = MockServer::start().await;
        let small = windowed("gpt-small", &[CHAT_ENDPOINT], 1.0, Some(1_000));
        let catalog = [
            small.clone(),
            windowed("gpt-big", &[CHAT_ENDPOINT], 1.0, Some(400_000)),
        ];
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &small,
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.1),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &catalog,
            NOW,
        )
        .await;

        // 40 000 bytes is 10 000 estimated tokens, past the 1 000-token window.
        let long_route = route_sized(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            40_000,
            &catalog,
            NOW + 60,
        )
        .await;
        let (text_model, _) = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["one"]),
            &catalog,
            NOW + 120,
        )
        .await;

        assert_eq!(long_route.model, "gpt-big");
        assert!(long_route.swap_reason.is_some());
        assert_eq!(text_model, "gpt-small");
    }

    #[test]
    fn image_in_an_earlier_turn_sets_has_image_through_the_shared_helper() {
        let body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
            ]},
            {"role": "assistant", "content": "a cat"},
            {"role": "user", "content": "and the next one?"},
        ]});

        let turn =
            Turn::parse(PROFILE, AutoTier::Balance, "/v1/messages?beta=true", &body).unwrap();

        assert!(turn.has_image);
        assert_eq!(turn.prompt.as_deref(), Some("and the next one?"));
    }

    #[test]
    fn text_only_turn_has_no_image() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "one"}]},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "two"},
        ]});

        let turn = Turn::parse(PROFILE, AutoTier::Balance, UPSTREAM_MESSAGES_PATH, &body).unwrap();

        assert!(!turn.has_image);
    }

    fn priced(id: &str, endpoints: &[&str], input: f64, output: f64) -> Value {
        json!({
            "id": id,
            "supported_endpoints": endpoints,
            "model_picker_enabled": true,
            "capabilities": {"type": "chat"},
            "billing": {
                "auto_discount": 0.1,
                "token_prices": {"default": {"input_price": input, "output_price": output}},
            },
        })
    }

    #[tokio::test]
    async fn incompatible_pick_uses_the_closest_priced_compatible_model() {
        let server = MockServer::start().await;
        let selected = priced("gpt-sol", &["/chat/completions"], 200.0, 1000.0);
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &selected,
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .mount(&server)
            .await;
        let catalog = [
            selected.clone(),
            priced(
                "claude-haiku",
                &["/v1/messages", "/chat/completions"],
                10.0,
                50.0,
            ),
            priced(
                "claude-sonnet",
                &["/v1/messages", "/chat/completions"],
                200.0,
                1000.0,
            ),
            priced("claude-opus", &["/v1/messages"], 1000.0, 5000.0),
            priced("gpt-terra", &["/chat/completions"], 200.0, 1200.0),
        ];
        let body = json!({
            "model": "copilot/auto",
            "system": "be terse",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "plan it"}]}],
        });
        let router = CopilotAutoRouter::new();

        let picked = route(
            &router,
            &server,
            UPSTREAM_MESSAGES_PATH,
            &body,
            &catalog,
            NOW,
        )
        .await;

        assert_eq!(
            picked,
            ("claude-sonnet".to_string(), Some("sess-1".to_string()))
        );
    }

    #[tokio::test]
    async fn auto_failure_falls_back_to_the_default_model_and_warns() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        let logs = CapturedLogs::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .finish(),
        );
        let router = CopilotAutoRouter::new();

        let picked = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["hi"]),
            &[],
            NOW,
        )
        .await;

        assert_eq!(picked, (DEFAULT_MODEL.to_string(), None));
        let output = logs.text();
        assert!(output.contains("WARN"), "{output}");
        assert!(output.contains("status 503"), "{output}");
        assert!(!output.contains(BEARER), "{output}");
    }

    #[tokio::test]
    async fn auto_failure_on_a_check_keeps_the_cached_decision() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(auto_response(
                &chat_model("gpt-small"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let turns = ["1", "2", "3", "4"];

        let mut last = None;
        for count in 1..=turns.len() {
            last = Some(
                route(
                    &router,
                    &server,
                    UPSTREAM_CHAT_PATH,
                    &chat_body(&turns[..count]),
                    &[],
                    NOW,
                )
                .await,
            );
        }

        assert_eq!(
            last,
            Some(("gpt-small".to_string(), Some("sess-1".to_string())))
        );
    }

    #[tokio::test]
    async fn tool_follow_up_on_an_unknown_conversation_uses_the_default_without_auto() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();

        let picked = route(
            &router,
            &server,
            UPSTREAM_CHAT_PATH,
            &tool_follow_up(&["x"]),
            &[],
            NOW,
        )
        .await;

        assert_eq!(picked, (DEFAULT_MODEL.to_string(), None));
    }

    #[test]
    fn tier_parses_auto_ids_and_retired_aliases() {
        let cases = [
            ("auto", Some(AutoTier::Balance)),
            ("auto-efficiency", Some(AutoTier::Efficiency)),
            ("auto-balance", Some(AutoTier::Balance)),
            ("auto-intelligence", Some(AutoTier::Intelligence)),
            ("auto-fast", Some(AutoTier::Fast)),
            ("auto-eco", Some(AutoTier::Efficiency)),
            ("auto-balanced", Some(AutoTier::Balance)),
            ("auto-max", Some(AutoTier::Intelligence)),
            ("auto-turbo", None),
            ("auto-", None),
            ("autobalance", None),
            ("gpt-4.1", None),
        ];

        for (id, tier) in cases {
            assert_eq!(AutoTier::from_model(id), tier, "{id}");
        }
        for tier in AutoTier::ALL {
            assert_eq!(AutoTier::from_model(&tier.model_id()), Some(tier));
        }
        assert_eq!(AutoTier::Efficiency.model_id(), "auto-efficiency");
        assert_eq!(json!(AutoTier::Intelligence), json!("intelligence"));
    }

    async fn route_on_tier(
        router: &CopilotAutoRouter,
        server: &MockServer,
        tier: AutoTier,
        body: &Value,
    ) -> AutoRoute {
        let client = reqwest::Client::new();
        let uri = server.uri();
        let upstream = AutoUpstream {
            client: &client,
            profile: PROFILE,
            bearer: BEARER,
            api_base: &uri,
        };
        let input = AutoInput {
            tier,
            path: UPSTREAM_CHAT_PATH,
            body,
            prompt_bytes: 0,
            catalog: &[],
            default_model: DEFAULT_MODEL,
        };
        router.route_at(&upstream, &input, NOW).await
    }

    #[tokio::test]
    async fn auto_body_sends_the_tier_and_a_tier_switch_routes_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(body_partial_json(json!({"tier": "efficiency"})))
            .respond_with(auto_response(
                &chat_model("gpt-cheap"),
                "sess-eco",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(body_partial_json(json!({"tier": "intelligence"})))
            .respond_with(auto_response(
                &chat_model("gpt-smart"),
                "sess-max",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();

        let efficient =
            route_on_tier(&router, &server, AutoTier::Efficiency, &chat_body(&["hi"])).await;
        let reused = route_on_tier(
            &router,
            &server,
            AutoTier::Efficiency,
            &chat_body(&["hi", "more"]),
        )
        .await;
        let smart = route_on_tier(
            &router,
            &server,
            AutoTier::Intelligence,
            &chat_body(&["hi", "more"]),
        )
        .await;

        assert_eq!(efficient.model, "gpt-cheap");
        assert_eq!(reused.model, "gpt-cheap");
        assert_eq!(reused.turn, AutoTurn::Reused);
        assert_eq!(smart.model, "gpt-smart");
        assert_eq!(smart.turn, AutoTurn::Routed);
        let tiers: Vec<Value> = auto_bodies(&server.received_requests().await.unwrap())
            .into_iter()
            .map(|body| body["tier"].clone())
            .collect();
        assert_eq!(tiers, [json!("efficiency"), json!("intelligence")]);
    }

    #[test]
    fn conversation_key_is_stable_across_turns_and_differs_by_system_prompt() {
        let first = Turn::parse(
            PROFILE,
            AutoTier::Balance,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a"]),
        )
        .unwrap();
        let later = Turn::parse(
            PROFILE,
            AutoTier::Balance,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a", "b", "c"]),
        )
        .unwrap();
        let mut other_system = chat_body(&["a"]);
        other_system["messages"][0]["content"] = json!("be verbose");
        let other = Turn::parse(
            PROFILE,
            AutoTier::Balance,
            UPSTREAM_CHAT_PATH,
            &other_system,
        )
        .unwrap();
        let other_profile = Turn::parse(
            "other",
            AutoTier::Balance,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a"]),
        )
        .unwrap();
        let other_tier = Turn::parse(
            PROFILE,
            AutoTier::Intelligence,
            UPSTREAM_CHAT_PATH,
            &chat_body(&["a"]),
        )
        .unwrap();

        assert_eq!(first.key, later.key);
        assert_ne!(first.key, other.key);
        assert_ne!(first.key, other_profile.key);
        assert_ne!(first.key, other_tier.key);
        assert_eq!(later.user_turns, 3);
        assert_eq!(later.prompt.as_deref(), Some("c"));
    }

    #[test]
    fn unpriced_catalog_uses_the_newest_model_of_the_complexity_family() {
        let unpriced = |id: &str, endpoints: &[&str]| {
            json!({"id": id, "supported_endpoints": endpoints, "model_picker_enabled": true,
                   "capabilities": {"type": "chat"}})
        };
        let catalog = [
            unpriced("claude-haiku-5-5", &["/v1/messages"]),
            unpriced("claude-sonnet-5", &["/v1/messages"]),
            unpriced("claude-sonnet-5-5", &["/v1/messages"]),
            unpriced("claude-opus-5-5", &["/v1/messages"]),
            unpriced("gpt-6-luna", &["/chat/completions"]),
        ];
        let selected = ModelTraits::from_entry(&catalog[4]).unwrap();
        let complexity = complexity(Some(
            "Auto selected GPT-6 Luna since the task has medium complexity.",
        ));

        let model = compatible_model(
            &selected,
            "/v1/messages",
            false,
            &catalog,
            complexity.map(|(_, family)| family),
            fit(0, &HashMap::new()),
        );

        assert_eq!(complexity, Some(("medium", "sonnet")));
        assert_eq!(
            model.map(|(id, _)| id).as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn trailing_system_message_does_not_hide_the_user_prompt() {
        let body = json!({"system": [{"type": "text", "text": "sys"}], "messages": [
            {"role": "user", "content": [{"type": "text", "text": "reply ok"}]},
            {"role": "system", "content": "late context"},
        ]});

        let turn = Turn::parse(PROFILE, AutoTier::Balance, UPSTREAM_MESSAGES_PATH, &body).unwrap();

        assert_eq!(turn.prompt.as_deref(), Some("reply ok"));
        assert_eq!(turn.user_turns, 1);
    }

    #[test]
    fn anthropic_tool_result_with_reminder_text_is_a_follow_up() {
        let body = json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "done"},
                {"type": "text", "text": "<system-reminder>keep going</system-reminder>"},
            ]},
        ]});

        let turn = Turn::parse(PROFILE, AutoTier::Balance, UPSTREAM_MESSAGES_PATH, &body).unwrap();

        assert_eq!(turn.prompt, None);
        assert_eq!(turn.user_turns, 1);
    }

    #[test]
    fn anthropic_tool_result_turn_has_no_prompt_and_image_is_detected() {
        let tool_turn = json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "done"}]},
        ]});
        let image_turn = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "what is this"}, {"type": "image"}]},
        ]});

        let tool = Turn::parse(
            PROFILE,
            AutoTier::Balance,
            UPSTREAM_MESSAGES_PATH,
            &tool_turn,
        )
        .unwrap();
        let image = Turn::parse(
            PROFILE,
            AutoTier::Balance,
            UPSTREAM_MESSAGES_PATH,
            &image_turn,
        )
        .unwrap();

        assert_eq!(tool.prompt, None);
        assert_eq!(tool.user_turns, 1);
        assert_eq!(image.prompt.as_deref(), Some("what is this"));
        assert!(image.has_image);
        assert_eq!(image.endpoint, MESSAGES_ENDPOINT);
    }

    #[test]
    fn responses_function_call_output_has_no_prompt_and_string_input_is_a_prompt() {
        let follow_up = json!({"instructions": "sys", "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "run it"}]},
            {"type": "function_call", "call_id": "c"},
            {"type": "function_call_output", "call_id": "c", "output": "ok"},
        ]});
        let plain = json!({"input": "summarise"});

        let tool = Turn::parse(PROFILE, AutoTier::Balance, "/responses", &follow_up).unwrap();
        let prompt = Turn::parse(PROFILE, AutoTier::Balance, "/responses", &plain).unwrap();

        assert_eq!(tool.prompt, None);
        assert_eq!(tool.user_turns, 1);
        assert_eq!(prompt.prompt.as_deref(), Some("summarise"));
        assert_eq!(prompt.endpoint, RESPONSES_ENDPOINT);
    }

    #[test]
    fn conversation_cache_is_bounded() {
        let router = CopilotAutoRouter::new();
        for index in 0..=MAX_CONVERSATIONS {
            router.slot(conversation_key(
                PROFILE,
                AutoTier::Balance,
                "",
                &index.to_string(),
            ));
        }
        let cache = router.conversations.lock().unwrap();
        assert_eq!(cache.slots.len(), MAX_CONVERSATIONS);
        assert!(
            !cache
                .slots
                .contains_key(&conversation_key(PROFILE, AutoTier::Balance, "", "0"))
        );
    }

    #[test]
    fn vision_request_skips_a_pick_without_vision() {
        let selected = ModelTraits::from_entry(&json!({
            "id": "text-only",
            "supported_endpoints": ["/chat/completions"],
            "capabilities": {"supports": {"vision": false}},
        }))
        .unwrap();
        let mut seeing = priced("sees", &["/chat/completions"], 1.0, 1.0);
        seeing["capabilities"]["supports"] = json!({"vision": true});
        let catalog = [priced("blind", &["/chat/completions"], 1.0, 1.0), seeing];

        let model = compatible_model(
            &selected,
            CHAT_ENDPOINT,
            true,
            &catalog,
            None,
            fit(0, &HashMap::new()),
        );

        assert_eq!(model, Some(("sees".to_string(), Some(SwapReason::Vision))));
    }

    const CLAUDE_CODE_PROMPT_BYTES: usize = 440_000;

    fn windowed(id: &str, endpoints: &[&str], price: f64, max_prompt_tokens: Option<u64>) -> Value {
        let mut entry = priced(id, endpoints, price, price);
        if let Some(limit) = max_prompt_tokens {
            entry["capabilities"]["limits"] = json!({"max_prompt_tokens": limit});
        }
        entry
    }

    fn traits(entry: &Value) -> ModelTraits {
        ModelTraits::from_entry(entry).unwrap()
    }

    /// Limits as if each was learned from a Copilot rejection.
    fn learned(limits: &[(&str, u64)]) -> HashMap<String, ModelLimit> {
        limits
            .iter()
            .map(|(id, limit)| {
                let entry = ModelLimit {
                    limit: *limit,
                    source: LimitSource::Learned,
                    learned_at: 0,
                };
                ((*id).to_string(), entry)
            })
            .collect()
    }

    fn fit(estimated_tokens: u64, limits: &HashMap<String, ModelLimit>) -> Fit<'_> {
        Fit {
            estimated_tokens,
            limits,
        }
    }

    #[test]
    fn large_prompt_swaps_past_a_too_small_closest_model() {
        let selected = windowed("gpt-sol", &["/chat/completions"], 10.0, Some(400_000));
        let catalog = [
            selected.clone(),
            windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000)),
            windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(200_000)),
            windowed("claude-opus-5-5", &["/v1/messages"], 500.0, Some(200_000)),
        ];
        let estimated = estimated_prompt_tokens(CLAUDE_CODE_PROMPT_BYTES);
        let limits = learned(&[
            ("claude-haiku-5-5", 100_000),
            ("claude-sonnet-5-5", 200_000),
            ("claude-opus-5-5", 200_000),
        ]);

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(estimated, &limits),
        );

        assert_eq!(estimated, 110_000);
        assert_eq!(
            model,
            Some((
                "claude-sonnet-5-5".to_string(),
                Some(SwapReason::Endpoint(MESSAGES_ENDPOINT))
            ))
        );
    }

    #[test]
    fn too_small_auto_pick_is_swapped_for_a_model_that_fits() {
        let selected = windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000));
        let catalog = [
            selected.clone(),
            windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(200_000)),
        ];
        let limits = learned(&[
            ("claude-haiku-5-5", 100_000),
            ("claude-sonnet-5-5", 200_000),
        ]);

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(110_000, &limits),
        );

        assert_eq!(
            model,
            Some((
                "claude-sonnet-5-5".to_string(),
                Some(SwapReason::PromptTooLarge {
                    estimated: 110_000,
                    limit: 100_000
                })
            ))
        );
    }

    #[test]
    fn responses_only_pick_is_kept_and_chosen_as_a_swap_on_v1_messages() {
        let selected = windowed("gpt-sol", &["/responses"], 10.0, None);
        let chat_only = windowed("gemini-pro", &["/chat/completions"], 10.0, None);
        let catalog = [
            selected.clone(),
            chat_only.clone(),
            windowed("gpt-terra", &["/responses"], 10.0, None),
        ];
        let pick = |model: &Value| {
            compatible_model(
                &traits(model),
                MESSAGES_ENDPOINT,
                false,
                &catalog,
                None,
                fit(0, &HashMap::new()),
            )
        };

        assert_eq!(pick(&selected), Some(("gpt-sol".to_string(), None)));
        assert_eq!(
            pick(&chat_only),
            Some((
                "gpt-sol".to_string(),
                Some(SwapReason::Endpoint(MESSAGES_ENDPOINT))
            ))
        );
    }

    #[test]
    fn when_nothing_fits_the_largest_window_wins() {
        let selected = windowed("gpt-sol", &["/chat/completions"], 10.0, None);
        let catalog = [
            selected.clone(),
            windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000)),
            windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(150_000)),
            windowed("claude-opus-5-5", &["/v1/messages"], 500.0, Some(120_000)),
        ];
        let limits = learned(&[
            ("claude-haiku-5-5", 100_000),
            ("claude-sonnet-5-5", 150_000),
            ("claude-opus-5-5", 120_000),
        ]);

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(500_000, &limits),
        );

        assert_eq!(
            model.map(|(id, _)| id).as_deref(),
            Some("claude-sonnet-5-5")
        );
    }

    #[test]
    fn too_small_pick_with_the_largest_window_is_kept() {
        let selected = windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(200_000));
        let catalog = [
            selected.clone(),
            windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000)),
        ];
        let limits = learned(&[
            ("claude-sonnet-5-5", 200_000),
            ("claude-haiku-5-5", 100_000),
        ]);

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(500_000, &limits),
        );

        assert_eq!(model, Some(("claude-sonnet-5-5".to_string(), None)));
    }

    #[test]
    fn unknown_prompt_limit_is_not_filtered() {
        let selected = windowed("gpt-sol", &["/chat/completions"], 10.0, None);
        let catalog = [
            selected.clone(),
            windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, None),
            windowed("claude-opus-5-5", &["/v1/messages"], 500.0, Some(1_000_000)),
        ];

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(110_000, &learned(&[("claude-opus-5-5", 1_000_000)])),
        );

        assert_eq!(model.map(|(id, _)| id).as_deref(), Some("claude-haiku-5-5"));
    }

    #[test]
    fn listed_limit_never_swaps_away_from_a_native_claude_pick() {
        let selected = windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000));
        let catalog = [
            selected.clone(),
            windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(200_000)),
        ];

        let model = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(110_000, &HashMap::new()),
        );

        assert_eq!(model, Some(("claude-haiku-5-5".to_string(), None)));
    }

    #[test]
    fn listed_limit_still_binds_off_v1_messages() {
        let selected = windowed("gpt-small", &["/responses"], 10.0, Some(100_000));
        let catalog = [
            selected.clone(),
            windowed("gpt-large", &["/responses"], 50.0, Some(400_000)),
        ];

        let model = compatible_model(
            &traits(&selected),
            RESPONSES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(110_000, &HashMap::new()),
        );

        assert_eq!(model.map(|(id, _)| id).as_deref(), Some("gpt-large"));
    }

    #[test]
    fn learned_limit_overrides_the_listed_one() {
        let selected = windowed("claude-opus-5.5", &["/v1/messages"], 500.0, Some(200_000));
        let catalog = [
            selected.clone(),
            windowed("gpt-large", &["/v1/messages"], 500.0, Some(400_000)),
        ];

        let roomy = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(300_000, &learned(&[("claude-opus-5.5", 1_000_000)])),
        );
        let tight = compatible_model(
            &traits(&selected),
            MESSAGES_ENDPOINT,
            false,
            &catalog,
            None,
            fit(300_000, &learned(&[("claude-opus-5.5", 250_000)])),
        );

        assert_eq!(roomy, Some(("claude-opus-5.5".to_string(), None)));
        assert_eq!(tight.map(|(id, _)| id).as_deref(), Some("gpt-large"));
    }

    #[test]
    fn swap_reason_text_names_the_cause() {
        let texts = [
            SwapReason::Endpoint(MESSAGES_ENDPOINT),
            SwapReason::Vision,
            SwapReason::PromptTooLarge {
                estimated: 110_000,
                limit: 100_000,
            },
        ]
        .map(|reason| reason.to_string());

        assert_eq!(
            texts,
            [
                "does not serve /v1/messages",
                "does not accept images",
                "allows 100000 prompt tokens, below the estimated 110000",
            ]
        );
    }

    #[tokio::test]
    async fn route_reports_the_pick_the_swap_and_the_complexity() {
        let server = MockServer::start().await;
        let selected = windowed("claude-haiku-5-5", &["/v1/messages"], 10.0, Some(100_000));
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "session_token": "sess-1",
                "expires_at": NOW + DAY_SECS,
                "selected_model": selected,
                "selection_reason": "Auto selected Haiku since the task has low complexity.",
                "hydra_scores": {},
                "discounted_costs": {},
            })))
            .mount(&server)
            .await;
        let catalog = [
            selected.clone(),
            windowed("claude-sonnet-5-5", &["/v1/messages"], 50.0, Some(200_000)),
        ];
        let body = json!({"messages": [{"role": "user", "content": "plan it"}]});
        let limits = ModelLimits::default();
        limits.learn(PROFILE, "claude-haiku-5-5", 100_000, 0);
        let router = CopilotAutoRouter::with_limits(Arc::new(limits));

        let route = route_sized(
            &router,
            &server,
            UPSTREAM_MESSAGES_PATH,
            &body,
            CLAUDE_CODE_PROMPT_BYTES,
            &catalog,
            NOW,
        )
        .await;

        assert_eq!(route.model, "claude-sonnet-5-5");
        assert_eq!(route.auto_pick, "claude-haiku-5-5");
        assert_eq!(
            route.swap_reason.as_deref(),
            Some(
                "Auto pick claude-haiku-5-5 allows 100000 prompt tokens, below the estimated 110000"
            )
        );
        assert_eq!(route.complexity, Some("low"));
        assert_eq!(
            route.selection_reason.as_deref(),
            Some("Auto selected Haiku since the task has low complexity.")
        );
        assert_eq!(route.estimated_prompt_tokens, 110_000);
        assert_eq!(route.turn, AutoTurn::Routed);
    }

    #[tokio::test]
    async fn turn_is_routed_then_reused_and_default_when_auto_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .and(body_partial_json(json!({"prompt": "fix the bug"})))
            .respond_with(auto_response(
                &chat_model("gpt-picked"),
                "sess-1",
                NOW + DAY_SECS,
                &scores(0.5),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/auto"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let router = CopilotAutoRouter::new();
        let mut turns = Vec::new();
        for body in [
            chat_body(&["fix the bug"]),
            tool_follow_up(&["fix the bug"]),
            chat_body(&["another conversation"]),
        ] {
            let route = route_sized(&router, &server, UPSTREAM_CHAT_PATH, &body, 0, &[], NOW).await;
            turns.push(route.turn);
        }

        assert_eq!(
            turns,
            [AutoTurn::Routed, AutoTurn::Reused, AutoTurn::Default]
        );
    }

    #[test]
    fn selection_reason_is_capped() {
        let long = "x".repeat(MAX_SELECTION_REASON_CHARS + 50);

        let capped = truncated_reason(Some(long)).unwrap();

        assert_eq!(capped.chars().count(), MAX_SELECTION_REASON_CHARS);
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn input_for<'a>(catalog: &'a [Value], body: &'a Value) -> AutoInput<'a> {
        AutoInput {
            tier: AutoTier::Intelligence,
            path: CHAT_ENDPOINT,
            body,
            prompt_bytes: 64,
            catalog,
            default_model: "m-plain",
        }
    }

    #[test]
    fn pick_effort_follows_the_catalog_and_keeps_a_supported_caller_level() {
        let catalog = vec![
            json!({"id": "m-levels", "capabilities": {"supports": {"reasoning_effort": ["low", "high"]}}}),
            json!({"id": "m-plain"}),
        ];
        let asked = json!({"messages": [{"role": "user", "content": "review the diff"}]});
        let explicit = json!({
            "reasoning_effort": "low",
            "messages": [{"role": "user", "content": "review the diff"}]
        });

        assert_eq!(pick_effort(&input_for(&catalog, &asked), "m-plain"), None);
        assert_eq!(
            pick_effort(&input_for(&catalog, &asked), "m-levels"),
            Some(EffortLevel::High)
        );
        assert_eq!(
            pick_effort(&input_for(&catalog, &explicit), "m-levels"),
            Some(EffortLevel::Low)
        );
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;

        fn make_writer(&'a self) -> CapturedLogs {
            self.clone()
        }
    }
}

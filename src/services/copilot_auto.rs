//! Copilot Auto routing: `POST {api}/auto` picks the model for a conversation,
//! and the server's multi-turn schedule says on which later user turns to ask again.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::schemas::copilot_auto_response::CopilotAutoResponse;

const AUTO_PATH: &str = "auto";
const HEADER_API_VERSION: &str = "x-github-api-version";
/// `/auto` rejects every other version with "invalid apiVersion".
const AUTO_API_VERSION: &str = "2026-08-01";
const AUTO_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_REFRESH_MARGIN_SECS: u64 = 5 * 60;
const MAX_CONVERSATIONS: usize = 256;
const CHAT_ENDPOINT: &str = "/chat/completions";
const RESPONSES_ENDPOINT: &str = "/responses";
const MESSAGES_ENDPOINT: &str = "/v1/messages";
const TEXT_PART_TYPES: [&str; 2] = ["text", "input_text"];
const TOOL_RESULT_PART_TYPE: &str = "tool_result";
const COMPLEXITY_FAMILIES: [(&str, &str); 3] = [
    ("low complexity", "haiku"),
    ("medium complexity", "sonnet"),
    ("high complexity", "opus"),
];
const ROLE_USER: &str = "user";
const ROLES_SYSTEM: [&str; 2] = ["system", "developer"];

type ConversationKey = [u8; 32];
type ConversationSlot = tokio::sync::Mutex<Option<Conversation>>;

/// Per-conversation Copilot Auto decisions, shared by every request so a
/// conversation keeps its model between the turns the schedule lets it re-route.
pub struct CopilotAutoRouter {
    conversations: Mutex<ConversationCache>,
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
    family_hint: Option<&'static str>,
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
    in_auto_pool: bool,
    usable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyShape {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

/// What one request body says about its conversation and its latest turn.
#[derive(Debug)]
struct Turn {
    key: ConversationKey,
    endpoint: &'static str,
    /// The latest user message text, or `None` for a tool-loop follow-up.
    prompt: Option<String>,
    has_image: bool,
    user_turns: usize,
}

/// One message-like item of a request body.
struct Item<'a> {
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
            .finish()
    }
}

impl CopilotAutoRouter {
    /// Creates a router with an empty conversation cache.
    pub fn new() -> CopilotAutoRouter {
        CopilotAutoRouter {
            conversations: Mutex::new(ConversationCache::default()),
        }
    }

    /// Returns the upstream model id for this request and the Copilot session
    /// token to send with it. `path` is the upstream path, which sets the body
    /// shape and the endpoint the model must support. `catalog` holds the
    /// profile's raw `/models` entries. When `/auto` fails and the conversation
    /// has no cached decision, the request goes to `default_model` with no token.
    #[allow(clippy::too_many_arguments)]
    pub async fn route(
        &self,
        client: &reqwest::Client,
        profile: &str,
        bearer: &str,
        api_base: &str,
        path: &str,
        body: &Value,
        catalog: &[Value],
        default_model: &str,
    ) -> (String, Option<String>) {
        let upstream = Upstream {
            client,
            profile,
            bearer,
            api_base,
        };
        self.route_at(
            &upstream,
            path,
            body,
            catalog,
            default_model,
            unix_now_secs(),
        )
        .await
    }

    async fn route_at(
        &self,
        upstream: &Upstream<'_>,
        path: &str,
        body: &Value,
        catalog: &[Value],
        default_model: &str,
        now_secs: u64,
    ) -> (String, Option<String>) {
        let fallback = || (default_model.to_string(), None);
        let Some(turn) = Turn::parse(upstream.profile, path, body) else {
            return fallback();
        };
        let slot = self.slot(turn.key);
        let mut state = slot.lock().await;
        let kind = match (state.as_ref(), turn.prompt.as_deref()) {
            (None, Some(_)) => Some(RouteKind::First),
            (Some(conversation), Some(_)) => conversation.route_kind(turn.user_turns, now_secs),
            (_, None) => None,
        };
        if let (Some(kind), Some(prompt)) = (kind, turn.prompt.as_deref()) {
            let request = auto_request_body(
                prompt,
                &turn,
                state.as_ref().filter(|_| kind == RouteKind::Check),
            );
            match fetch_decision(upstream, &request).await {
                Ok(decision) => apply_decision(
                    &mut state,
                    kind,
                    decision,
                    turn.user_turns,
                    upstream.profile,
                ),
                Err(e) => tracing::warn!(
                    "profile {}: Copilot Auto routing failed, so the request uses the cached or default model: {e}",
                    upstream.profile
                ),
            }
        }
        let Some(conversation) = state.as_ref() else {
            return fallback();
        };
        let Some(model) = compatible_model(
            &conversation.model,
            turn.endpoint,
            turn.has_image,
            catalog,
            conversation.family_hint,
        ) else {
            tracing::warn!(
                "profile {}: no Copilot model serves {} for the Auto pick {}, so the request uses the default model",
                upstream.profile,
                turn.endpoint,
                conversation.model.id
            );
            return fallback();
        };
        let token =
            (now_secs < conversation.expires_at).then(|| conversation.session_token.clone());
        (model, token)
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

/// Where and as whom `/auto` is called.
struct Upstream<'a> {
    client: &'a reqwest::Client,
    profile: &'a str,
    bearer: &'a str,
    api_base: &'a str,
}

impl Conversation {
    fn anchored(decision: CopilotAutoResponse, model: ModelTraits, turn: usize) -> Conversation {
        let schedule = parse_schedule(decision.multi_turn.as_ref());
        let family_hint = complexity_family(decision.selection_reason.as_deref());
        Conversation {
            model,
            session_token: decision.session_token,
            expires_at: decision.expires_at,
            anchor_turn: turn,
            checked_turn: turn,
            anchor_scores: decision.hydra_scores,
            skip_window: schedule.as_ref().map_or(0, |s| s.initial_skip),
            schedule,
            family_hint,
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

    /// Whether the new task-type scores moved away from the anchor by at least
    /// the schedule's threshold, measured in each task type's sigma. Without
    /// comparable scores, a different model counts as escalation.
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
        self.session_token = decision.session_token;
        self.expires_at = decision.expires_at;
        self.checked_turn = turn;
    }
}

fn apply_decision(
    state: &mut Option<Conversation>,
    kind: RouteKind,
    decision: CopilotAutoResponse,
    turn: usize,
    profile: &str,
) {
    let Some(model) = ModelTraits::from_entry(&decision.selected_model) else {
        tracing::warn!("profile {profile}: the Copilot Auto response named no selected model");
        return;
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
        && !conversation.escalates(&decision, &model)
    {
        conversation.back_off(decision, turn);
        return;
    }
    *state = Some(Conversation::anchored(decision, model, turn));
}

fn parse_schedule(multi_turn: Option<&Value>) -> Option<Schedule> {
    multi_turn.and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// The `/auto` body: the prompt, the image flag, and on a schedule check the
/// conversation's multi-turn state.
fn auto_request_body(prompt: &str, turn: &Turn, conversation: Option<&Conversation>) -> Value {
    let mut body = Map::new();
    body.insert("prompt".to_string(), Value::from(prompt));
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
    upstream: &Upstream<'_>,
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

/// The model that serves `endpoint`: the Auto pick when it supports it,
/// otherwise the catalog model with the closest price that does. Without
/// prices, it uses the model family that matches Auto's complexity verdict.
fn compatible_model(
    selected: &ModelTraits,
    endpoint: &str,
    has_image: bool,
    catalog: &[Value],
    family_hint: Option<&str>,
) -> Option<String> {
    let models: Vec<ModelTraits> = catalog.iter().filter_map(ModelTraits::from_entry).collect();
    let listed = models.iter().find(|model| model.id == selected.id);
    let endpoints = selected
        .endpoints
        .as_ref()
        .or_else(|| listed.and_then(|model| model.endpoints.as_ref()));
    let serves_endpoint = endpoints.is_none_or(|list| list.iter().any(|e| e == endpoint));
    let serves_image = !has_image || selected.vision != Some(false);
    if serves_endpoint && serves_image {
        return Some(selected.id.clone());
    }
    let candidates: Vec<&ModelTraits> = models
        .iter()
        .filter(|model| model.usable && model.id != selected.id)
        .filter(|model| model.supports(endpoint))
        .filter(|model| !has_image || model.vision == Some(true))
        .collect();
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
    let Some(target) = target.filter(|_| pool.iter().any(|model| model.price.is_some())) else {
        return newest_in_family(&pool, family_hint)
            .or_else(|| pool.first().map(|model| model.id.clone()));
    };
    pool.into_iter()
        .min_by(|a, b| price_distance(a.price, target).total_cmp(&price_distance(b.price, target)))
        .map(|model| model.id.clone())
}

/// Auto's `selection_reason` names the task complexity; map it to a model family.
fn complexity_family(reason: Option<&str>) -> Option<&'static str> {
    let reason = reason?.to_ascii_lowercase();
    COMPLEXITY_FAMILIES
        .iter()
        .find(|(complexity, _)| reason.contains(complexity))
        .map(|(_, family)| *family)
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
            in_auto_pool: billing.and_then(|b| b.get("auto_discount")).is_some(),
            usable,
        })
    }

    fn supports(&self, endpoint: &str) -> bool {
        self.endpoints
            .as_ref()
            .is_some_and(|list| list.iter().any(|e| e == endpoint))
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
        let path = path.split_once('?').map_or(path, |(bare, _)| bare);
        let path = path.trim_end_matches('/');
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

    fn image_part_type(self) -> &'static str {
        match self {
            BodyShape::ChatCompletions => "image_url",
            BodyShape::Responses => "input_image",
            BodyShape::AnthropicMessages => "image",
        }
    }
}

impl Turn {
    fn parse(profile: &str, path: &str, body: &Value) -> Option<Turn> {
        let shape = BodyShape::from_path(path)?;
        let items = items(shape, body);
        let system = system_text(shape, body, &items);
        let user_texts: Vec<String> = items.iter().filter_map(Item::user_turn_text).collect();
        let latest = items
            .iter()
            .rev()
            .find(|item| !ROLES_SYSTEM.contains(&item.role));
        let prompt = latest.and_then(Item::user_turn_text);
        let has_image =
            latest.is_some_and(|item| content_has_image(item.content, shape.image_part_type()));
        let first_user = user_texts.first().map_or("", String::as_str);
        Some(Turn {
            key: conversation_key(profile, &system, first_user),
            endpoint: shape.endpoint(),
            prompt,
            has_image,
            user_turns: user_texts.len(),
        })
    }
}

fn items(shape: BodyShape, body: &Value) -> Vec<Item<'_>> {
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

fn system_text(shape: BodyShape, body: &Value, items: &[Item<'_>]) -> String {
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

fn content_has_image(content: &Value, image_type: &str) -> bool {
    let Value::Array(parts) = content else {
        return false;
    };
    parts.iter().any(|part| {
        part.get("type").and_then(Value::as_str) == Some(image_type)
            || part
                .get("content")
                .is_some_and(|nested| content_has_image(nested, image_type))
    })
}

/// Stable across the turns of one conversation: the profile, the system prompt
/// and the first user message, each length-prefixed so the parts cannot run together.
fn conversation_key(profile: &str, system: &str, first_user: &str) -> ConversationKey {
    let mut hasher = Sha256::new();
    for part in [profile, system, first_user] {
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
        let client = reqwest::Client::new();
        let uri = server.uri();
        let upstream = Upstream {
            client: &client,
            profile: PROFILE,
            bearer: BEARER,
            api_base: &uri,
        };
        router
            .route_at(&upstream, upstream_path, body, catalog, DEFAULT_MODEL, now)
            .await
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
        assert_eq!(bodies[0], json!({"prompt": "fix the bug"}));
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
            picked.push(
                route(&router, &server, UPSTREAM_CHAT_PATH, &body, &[], NOW)
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
        let selected = priced("gpt-sol", &["/responses"], 200.0, 1000.0);
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
            priced("gpt-terra", &["/responses"], 200.0, 1200.0),
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
    fn conversation_key_is_stable_across_turns_and_differs_by_system_prompt() {
        let first = Turn::parse(PROFILE, UPSTREAM_CHAT_PATH, &chat_body(&["a"])).unwrap();
        let later = Turn::parse(PROFILE, UPSTREAM_CHAT_PATH, &chat_body(&["a", "b", "c"])).unwrap();
        let mut other_system = chat_body(&["a"]);
        other_system["messages"][0]["content"] = json!("be verbose");
        let other = Turn::parse(PROFILE, UPSTREAM_CHAT_PATH, &other_system).unwrap();
        let other_profile = Turn::parse("other", UPSTREAM_CHAT_PATH, &chat_body(&["a"])).unwrap();

        assert_eq!(first.key, later.key);
        assert_ne!(first.key, other.key);
        assert_ne!(first.key, other_profile.key);
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
            unpriced("gpt-6-luna", &["/responses"]),
        ];
        let selected = ModelTraits::from_entry(&catalog[4]).unwrap();
        let family = complexity_family(Some(
            "Auto selected GPT-6 Luna since the task has medium complexity.",
        ));

        let model = compatible_model(&selected, "/v1/messages", false, &catalog, family);

        assert_eq!(family, Some("sonnet"));
        assert_eq!(model.as_deref(), Some("claude-sonnet-5-5"));
    }

    #[test]
    fn trailing_system_message_does_not_hide_the_user_prompt() {
        let body = json!({"system": [{"type": "text", "text": "sys"}], "messages": [
            {"role": "user", "content": [{"type": "text", "text": "reply ok"}]},
            {"role": "system", "content": "late context"},
        ]});

        let turn = Turn::parse(PROFILE, UPSTREAM_MESSAGES_PATH, &body).unwrap();

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

        let turn = Turn::parse(PROFILE, UPSTREAM_MESSAGES_PATH, &body).unwrap();

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

        let tool = Turn::parse(PROFILE, UPSTREAM_MESSAGES_PATH, &tool_turn).unwrap();
        let image = Turn::parse(PROFILE, UPSTREAM_MESSAGES_PATH, &image_turn).unwrap();

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

        let tool = Turn::parse(PROFILE, "/responses", &follow_up).unwrap();
        let prompt = Turn::parse(PROFILE, "/responses", &plain).unwrap();

        assert_eq!(tool.prompt, None);
        assert_eq!(tool.user_turns, 1);
        assert_eq!(prompt.prompt.as_deref(), Some("summarise"));
        assert_eq!(prompt.endpoint, RESPONSES_ENDPOINT);
    }

    #[test]
    fn conversation_cache_is_bounded() {
        let router = CopilotAutoRouter::new();
        for index in 0..=MAX_CONVERSATIONS {
            router.slot(conversation_key(PROFILE, "", &index.to_string()));
        }
        let cache = router.conversations.lock().unwrap();
        assert_eq!(cache.slots.len(), MAX_CONVERSATIONS);
        assert!(
            !cache
                .slots
                .contains_key(&conversation_key(PROFILE, "", "0"))
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

        let model = compatible_model(&selected, CHAT_ENDPOINT, true, &catalog, None);

        assert_eq!(model.as_deref(), Some("sees"));
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

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;

        fn make_writer(&'a self) -> CapturedLogs {
            self.clone()
        }
    }
}

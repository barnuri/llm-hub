//! The request/response transform seam.
//!
//! Everything here runs on the **`OpenAI`-shaped** body, after the Anthropic
//! route has already translated its request (`attempt_loop` translates first,
//! then calls [`apply_request`]). That ordering is the whole design: the
//! Anthropic `system` field has become an `OpenAI` `system` message by the time
//! a transform sees it, so one implementation covers both client-facing API
//! shapes and this module never learns that `/v1/messages` exists.
//!
//! Transforms are **header-driven, never profile-driven**. The seam runs once,
//! before the fallback chain is built, while the profile is only known per
//! attempt — so a profile-conditional transform would make the primary and the
//! fallback receive different request bodies and the `x-llm-hub-attempts` trail
//! would stop describing one comparable request. Header-driven keeps every
//! attempt byte-identical.
//!
//! Every transform is opt-in and fails open: an unrecognized header value, a
//! body that is not a JSON object, or a field of the wrong type leaves the
//! request untouched rather than erroring.

use axum::http::HeaderMap;
use serde_json::{Map, Value};

use crate::consts::{HEADER_REASONING_STRIP, HEADER_SYSTEM_PROMPT_MODE, SYSTEM_PROMPT_MAX_CHARS};
use crate::services::body_transform::BodyTransform;
use crate::services::tool_names::{self, NameMap};

/// Top-level request keys the reasoning strip removes.
const REASONING_KEYS: [&str; 3] = ["reasoning", "reasoning_effort", "thinking"];
/// Roles the system-prompt transform considers a system prompt.
const SYSTEM_ROLES: [&str; 2] = ["system", "developer"];

/// Which client-facing API shape a request arrived in. Decides body
/// translation and the response transform; the fallback chain, retry policy,
/// timeout handling and stats are identical for both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteKind {
    OpenAi,
    Anthropic,
}

/// What `x-llm-hub-system-prompt-mode` asked for. Anything else — including an
/// absent header — is not a mode at all and leaves the body untouched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SystemPromptMode {
    /// Explicit no-op, useful for overriding a client-side default.
    None,
    /// Cap each system prompt at [`SYSTEM_PROMPT_MAX_CHARS`] characters.
    Truncate,
    /// Remove system/developer messages entirely.
    Drop,
}

impl SystemPromptMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "none" => Some(Self::None),
            "truncate" => Some(Self::Truncate),
            "drop" => Some(Self::Drop),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Truncate => "truncate",
            Self::Drop => "drop",
        }
    }
}

/// What [`apply_request`] did, and what the response side therefore needs.
///
/// The default value means "nothing fired", which is what keeps the untouched
/// path free of any response-side work: `response_transform` answers `None` and
/// the body streams through byte-for-byte.
#[derive(Debug, Default)]
pub struct TransformPlan {
    /// Alias -> original tool name. Empty in the common case; a non-empty map
    /// is what obliges the response side to read the body back.
    pub names: NameMap,
    /// Inject `role:"assistant"` into the first streamed delta that omits it.
    pub inject_stream_role: bool,
    /// The request asked for a stream.
    pub is_stream: bool,
    /// Human-readable trail for the `x-llm-hub-transforms` response header.
    pub applied: Vec<String>,
}

impl TransformPlan {
    /// True when the winning response body has to be read and rewritten, which
    /// is also what decides whether the upstream is asked for `identity`
    /// content-encoding.
    #[must_use]
    pub fn wants_response_transform(&self) -> bool {
        self.inject_stream_role || !self.names.is_empty()
    }

    /// Value for the diagnostic `x-llm-hub-transforms` response header, or
    /// `None` when no request transform fired.
    #[must_use]
    pub fn header_value(&self) -> Option<String> {
        if self.applied.is_empty() {
            return None;
        }
        Some(self.applied.join(", "))
    }
}

/// The single request-side seam. Mutates the `OpenAI`-shaped body in place and
/// returns what the response side needs. Called exactly once per request, from
/// `attempt_loop`, before the fallback chain is built — so every attempt sends
/// the same bytes.
///
/// `stream_role_inject` is the `LLM_HUB_STREAM_ROLE` kill switch: role
/// injection is the one transform with no request header, because the clients
/// it fixes cannot set one.
pub fn apply_request(
    body: &mut Option<Value>,
    headers: &HeaderMap,
    stream_role_inject: bool,
) -> TransformPlan {
    let mut plan = TransformPlan::default();
    let Some(Value::Object(obj)) = body.as_mut() else {
        return plan;
    };

    plan.is_stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    plan.inject_stream_role = stream_role_inject && plan.is_stream;

    if let Some(mode) =
        header_str(headers, HEADER_SYSTEM_PROMPT_MODE).and_then(SystemPromptMode::parse)
        && apply_system_prompt(obj, mode)
    {
        plan.applied.push(format!("system-prompt={}", mode.label()));
    }

    plan.names = tool_names::truncate_in_body(obj);
    if !plan.names.is_empty() {
        plan.applied
            .push(format!("tool-names={}", plan.names.len()));
    }

    if header_is_truthy(headers, HEADER_REASONING_STRIP) && strip_reasoning(obj) {
        plan.applied.push("reasoning-strip".to_string());
    }

    plan
}

/// Builds the response-side transform for the winning upstream response.
///
/// `None` means byte-for-byte passthrough with no copy and no framing — which
/// is where every request that asked for nothing must land.
///
/// The `Anthropic` non-SSE arm is `None` because that translation buffers the
/// whole (non-streaming) body in the route rather than streaming it through a
/// `BodyTransform`; `deliver` never routes it here, and restores tool names
/// there instead.
#[must_use]
pub fn response_transform(
    route: RouteKind,
    plan: &TransformPlan,
    served_model: &str,
    is_sse: bool,
) -> Option<BodyTransform> {
    match (route, is_sse) {
        (RouteKind::Anthropic, true) => Some(BodyTransform::anthropic_stream(
            served_model.to_string(),
            plan.names.clone(),
        )),
        (RouteKind::OpenAi, true) if plan.inject_stream_role || !plan.names.is_empty() => Some(
            BodyTransform::sse(plan.inject_stream_role, plan.names.clone()),
        ),
        // A buffered body only ever needs the names put back, and a non-SSE
        // body is the one thing a transform is allowed to hold in full.
        (RouteKind::OpenAi, false) if !plan.names.is_empty() => {
            Some(BodyTransform::name_restore(plan.names.clone()))
        }
        _ => None,
    }
}

// --- feature 2: system-prompt trim ---

/// Returns true when the body actually changed.
fn apply_system_prompt(obj: &mut Map<String, Value>, mode: SystemPromptMode) -> bool {
    let Some(Value::Array(messages)) = obj.get_mut("messages") else {
        return false;
    };
    match mode {
        SystemPromptMode::None => false,
        SystemPromptMode::Drop => drop_system_messages(messages),
        SystemPromptMode::Truncate => {
            let mut changed = false;
            for message in messages.iter_mut().filter(|m| is_system_message(m)) {
                changed |= truncate_message(message, SYSTEM_PROMPT_MAX_CHARS);
            }
            changed
        }
    }
}

/// Deliberate deviation from the Go original: dropping *every* message would
/// guarantee an upstream 400, and fail-open means not manufacturing one.
fn drop_system_messages(messages: &mut Vec<Value>) -> bool {
    let removable = messages.iter().filter(|m| is_system_message(m)).count();
    if removable == 0 {
        return false;
    }
    if removable == messages.len() {
        tracing::warn!("system-prompt=drop would leave no messages; skipping");
        return false;
    }
    messages.retain(|m| !is_system_message(m));
    true
}

fn is_system_message(message: &Value) -> bool {
    message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| SYSTEM_ROLES.contains(&role))
}

fn truncate_message(message: &mut Value, budget: usize) -> bool {
    match message.get_mut("content") {
        Some(Value::String(text)) => match truncate_text(text, budget) {
            Some(cut) => {
                *text = cut;
                true
            }
            None => false,
        },
        Some(Value::Array(parts)) => truncate_parts(parts, budget),
        _ => false,
    }
}

/// Array content shares one budget across its `text` parts, in order: the part
/// that crosses the cap is cut, later text parts are removed. Parts of other
/// kinds are left alone — an image in a system message is not prose and
/// dropping it would lose information the cap was never about.
fn truncate_parts(parts: &mut Vec<Value>, budget: usize) -> bool {
    let mut remaining = budget;
    let mut changed = false;
    let mut drop_indices: Vec<usize> = Vec::new();

    for (index, part) in parts.iter_mut().enumerate() {
        let Some(Value::String(text)) = part.get_mut("text") else {
            continue;
        };
        if remaining == 0 {
            drop_indices.push(index);
            changed = true;
            continue;
        }
        let count = text.chars().count();
        if let Some(cut) = truncate_text(text, remaining) {
            *text = cut;
            changed = true;
        }
        remaining -= count.min(remaining);
    }

    for index in drop_indices.into_iter().rev() {
        parts.remove(index);
    }
    changed
}

/// Truncates to `max` **characters**, not bytes. The Go original byte-slices
/// here, which either splits a rune or (in Rust) panics outright; counting
/// characters is both safe and what the cap was always meant to express.
/// `None` when the text already fits.
fn truncate_text(text: &str, max: usize) -> Option<String> {
    if text.chars().count() <= max {
        return None;
    }
    Some(text.chars().take(max).collect())
}

// --- feature 5: reasoning strip ---

/// Removes the reasoning-shaped request parameters some upstreams reject
/// outright (llama.cpp, LM Studio, and a few corporate gateways 400 on
/// parameters they do not model, and the caller often cannot remove them
/// because an SDK adds them).
fn strip_reasoning(obj: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    for key in REASONING_KEYS {
        changed |= obj.remove(key).is_some();
    }
    changed
}

// --- header helpers ---

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn header_is_truthy(headers: &HeaderMap, name: &str) -> bool {
    header_str(headers, name).is_some_and(|raw| {
        matches!(
            raw.trim().to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in pairs {
            map.insert(
                key.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    // The `Option` is the shape `apply_request` takes, not a fallible result.
    #[allow(clippy::unnecessary_wraps, clippy::needless_pass_by_value)]
    fn chat(messages: Value) -> Option<Value> {
        Some(json!({"model": "p/m", "messages": messages}))
    }

    fn system_and_user(system: &str) -> Option<Value> {
        chat(json!([
            {"role": "system", "content": system},
            {"role": "user", "content": "hi"},
        ]))
    }

    fn mode_headers(value: &str) -> HeaderMap {
        headers(&[(HEADER_SYSTEM_PROMPT_MODE, value)])
    }

    #[test]
    fn mode_parses_case_insensitively_and_trims() {
        assert_eq!(
            SystemPromptMode::parse("  DROP "),
            Some(SystemPromptMode::Drop)
        );
        assert_eq!(
            SystemPromptMode::parse("Truncate"),
            Some(SystemPromptMode::Truncate)
        );
        assert_eq!(
            SystemPromptMode::parse("none"),
            Some(SystemPromptMode::None)
        );
        assert_eq!(SystemPromptMode::parse("nope"), None);
        assert_eq!(SystemPromptMode::parse(""), None);
    }

    #[test]
    fn unrecognized_or_absent_mode_leaves_body_untouched() {
        for header in [
            headers(&[]),
            mode_headers("shorten"),
            mode_headers(""),
            mode_headers("DROPPED"),
        ] {
            let mut body = system_and_user(&"x".repeat(5000));
            let before = body.clone();
            let plan = apply_request(&mut body, &header, true);
            assert_eq!(body, before, "body must be untouched");
            assert!(plan.applied.is_empty());
        }
    }

    #[test]
    fn drop_removes_system_and_developer_messages() {
        let mut body = chat(json!([
            {"role": "system", "content": "sys"},
            {"role": "developer", "content": "dev"},
            {"role": "user", "content": "hi"},
        ]));
        let plan = apply_request(&mut body, &mode_headers("drop"), true);
        let messages = body.as_ref().unwrap()["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(plan.applied, vec!["system-prompt=drop".to_string()]);
    }

    #[test]
    fn drop_is_skipped_when_it_would_empty_messages() {
        let mut body = chat(json!([{"role": "system", "content": "sys"}]));
        let before = body.clone();
        let plan = apply_request(&mut body, &mode_headers("drop"), true);
        assert_eq!(body, before);
        assert!(plan.applied.is_empty());
    }

    #[test]
    fn drop_without_system_messages_reports_nothing() {
        let mut body = chat(json!([{"role": "user", "content": "hi"}]));
        let before = body.clone();
        let plan = apply_request(&mut body, &mode_headers("drop"), true);
        assert_eq!(body, before);
        assert!(plan.applied.is_empty());
    }

    #[test]
    fn truncate_caps_string_content_at_the_char_limit() {
        let mut body = system_and_user(&"a".repeat(SYSTEM_PROMPT_MAX_CHARS + 500));
        let plan = apply_request(&mut body, &mode_headers("truncate"), true);
        let system = body.as_ref().unwrap()["messages"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(system.chars().count(), SYSTEM_PROMPT_MAX_CHARS);
        assert_eq!(body.as_ref().unwrap()["messages"][1]["content"], "hi");
        assert_eq!(plan.applied, vec!["system-prompt=truncate".to_string()]);
    }

    #[test]
    fn truncate_is_noop_below_the_cap() {
        let mut body = system_and_user("short prompt");
        let before = body.clone();
        let plan = apply_request(&mut body, &mode_headers("truncate"), true);
        assert_eq!(body, before);
        assert!(plan.applied.is_empty());
    }

    #[test]
    fn truncate_respects_char_boundaries_on_multibyte_text() {
        // Every char is 3 bytes: a byte slice at the cap would split a rune.
        let text = "あ".repeat(SYSTEM_PROMPT_MAX_CHARS + 10);
        let mut body = system_and_user(&text);
        apply_request(&mut body, &mode_headers("truncate"), true);
        let system = body.as_ref().unwrap()["messages"][0]["content"]
            .as_str()
            .unwrap();
        assert_eq!(system.chars().count(), SYSTEM_PROMPT_MAX_CHARS);
        assert_eq!(system.len(), SYSTEM_PROMPT_MAX_CHARS * 3);
        assert!(system.chars().all(|c| c == 'あ'));
    }

    #[test]
    fn truncate_caps_array_content_cumulatively_across_text_parts() {
        let mut body = chat(json!([
            {"role": "system", "content": [
                {"type": "text", "text": "a".repeat(SYSTEM_PROMPT_MAX_CHARS - 5)},
                {"type": "text", "text": "b".repeat(50)},
                {"type": "text", "text": "c"},
            ]},
            {"role": "user", "content": "hi"},
        ]));
        apply_request(&mut body, &mode_headers("truncate"), true);
        let parts = body.as_ref().unwrap()["messages"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2, "the part past the budget is removed");
        assert_eq!(
            parts[0]["text"].as_str().unwrap().chars().count(),
            SYSTEM_PROMPT_MAX_CHARS - 5
        );
        assert_eq!(parts[1]["text"], "bbbbb");
    }

    #[test]
    fn truncate_leaves_non_text_parts_alone() {
        let mut body = chat(json!([
            {"role": "system", "content": [
                {"type": "text", "text": "x".repeat(SYSTEM_PROMPT_MAX_CHARS + 1)},
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}},
            ]},
            {"role": "user", "content": "hi"},
        ]));
        apply_request(&mut body, &mode_headers("truncate"), true);
        let parts = body.as_ref().unwrap()["messages"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn none_mode_is_an_explicit_noop() {
        let mut body = system_and_user(&"a".repeat(5000));
        let before = body.clone();
        let plan = apply_request(&mut body, &mode_headers("none"), true);
        assert_eq!(body, before);
        assert!(plan.applied.is_empty());
    }

    #[test]
    fn reasoning_strip_truthy_values_remove_reasoning_keys() {
        for value in ["true", "1", "YES", " on "] {
            let mut body = Some(json!({
                "model": "p/m",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning": {"effort": "high"},
                "reasoning_effort": "high",
                "thinking": {"type": "enabled"},
            }));
            let plan = apply_request(
                &mut body,
                &headers(&[(HEADER_REASONING_STRIP, value)]),
                true,
            );
            let obj = body.as_ref().unwrap().as_object().unwrap();
            assert!(!obj.contains_key("reasoning"), "value={value}");
            assert!(!obj.contains_key("reasoning_effort"), "value={value}");
            assert!(!obj.contains_key("thinking"), "value={value}");
            assert!(obj.contains_key("messages"));
            assert_eq!(plan.applied, vec!["reasoning-strip".to_string()]);
        }
    }

    #[test]
    fn reasoning_strip_absent_or_falsey_leaves_body_untouched() {
        for header in [headers(&[]), headers(&[(HEADER_REASONING_STRIP, "false")])] {
            let mut body = Some(json!({
                "model": "p/m",
                "messages": [{"role": "user", "content": "hi"}],
                "reasoning_effort": "high",
            }));
            let before = body.clone();
            let plan = apply_request(&mut body, &header, true);
            assert_eq!(body, before);
            assert!(plan.applied.is_empty());
        }
    }

    #[test]
    fn reasoning_strip_reports_nothing_when_no_key_is_present() {
        let mut body = chat(json!([{"role": "user", "content": "hi"}]));
        let before = body.clone();
        let plan = apply_request(
            &mut body,
            &headers(&[(HEADER_REASONING_STRIP, "true")]),
            true,
        );
        assert_eq!(body, before);
        assert!(plan.applied.is_empty());
    }

    #[test]
    fn apply_request_on_a_bare_body_returns_an_empty_plan() {
        let mut body: Option<Value> = None;
        let plan = apply_request(&mut body, &mode_headers("drop"), true);
        assert!(!plan.is_stream);
        assert!(!plan.inject_stream_role);
        assert!(plan.applied.is_empty());
        assert!(!plan.wants_response_transform());

        let mut not_an_object = Some(json!(["nope"]));
        let plan = apply_request(&mut not_an_object, &mode_headers("drop"), true);
        assert!(plan.applied.is_empty());
        assert_eq!(not_an_object, Some(json!(["nope"])));
    }

    #[test]
    fn stream_role_is_planned_only_for_streaming_requests() {
        let mut streaming = Some(json!({"model": "p/m", "stream": true, "messages": []}));
        let plan = apply_request(&mut streaming, &headers(&[]), true);
        assert!(plan.is_stream);
        assert!(plan.inject_stream_role);
        assert!(plan.wants_response_transform());

        let mut buffered = Some(json!({"model": "p/m", "messages": []}));
        let plan = apply_request(&mut buffered, &headers(&[]), true);
        assert!(!plan.is_stream);
        assert!(!plan.inject_stream_role);
        assert!(!plan.wants_response_transform());
    }

    #[test]
    fn stream_role_kill_switch_disables_the_plan() {
        let mut body = Some(json!({"model": "p/m", "stream": true, "messages": []}));
        let plan = apply_request(&mut body, &headers(&[]), false);
        assert!(plan.is_stream);
        assert!(!plan.inject_stream_role);
        assert!(!plan.wants_response_transform());
    }

    #[test]
    fn applied_trail_lists_only_transforms_that_fired() {
        let mut body = system_and_user(&"a".repeat(5000));
        let plan = apply_request(
            &mut body,
            &headers(&[
                (HEADER_SYSTEM_PROMPT_MODE, "truncate"),
                (HEADER_REASONING_STRIP, "true"),
            ]),
            true,
        );
        // reasoning-strip found nothing to remove, so only the trim is listed.
        assert_eq!(plan.applied, vec!["system-prompt=truncate".to_string()]);
        assert_eq!(
            plan.header_value(),
            Some("system-prompt=truncate".to_string())
        );

        let empty = TransformPlan::default();
        assert_eq!(empty.header_value(), None);
    }

    #[test]
    fn response_transform_returns_none_for_plain_openai_traffic() {
        let plan = TransformPlan::default();
        assert!(response_transform(RouteKind::OpenAi, &plan, "p/m", false).is_none());
        assert!(response_transform(RouteKind::OpenAi, &plan, "p/m", true).is_none());
    }

    #[test]
    fn response_transform_selects_each_variant_per_the_decision_table() {
        let streaming = TransformPlan {
            inject_stream_role: true,
            is_stream: true,
            ..TransformPlan::default()
        };
        assert!(matches!(
            response_transform(RouteKind::OpenAi, &streaming, "p/m", true),
            Some(BodyTransform::Sse(_))
        ));
        // A streaming request answered with a JSON error is not an SSE body.
        assert!(response_transform(RouteKind::OpenAi, &streaming, "p/m", false).is_none());
        assert!(matches!(
            response_transform(RouteKind::Anthropic, &streaming, "p/m", true),
            Some(BodyTransform::AnthropicStream(_))
        ));
        assert!(response_transform(RouteKind::Anthropic, &streaming, "p/m", false).is_none());
    }

    // --- feature 3: tool-name truncation ---

    /// 79 bytes: the MCP double-prefix shape that motivates the transform.
    const LONG_TOOL: &str =
        "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";

    // The `Option` is the shape `apply_request` takes, not a fallible result.
    #[allow(clippy::unnecessary_wraps)]
    fn chat_with_tool(name: &str, stream: bool) -> Option<Value> {
        Some(json!({
            "model": "p/m",
            "stream": stream,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": name}}],
        }))
    }

    #[test]
    fn long_tool_names_are_truncated_and_reported_in_the_trail() {
        let mut body = chat_with_tool(LONG_TOOL, false);
        let plan = apply_request(&mut body, &headers(&[]), true);
        let alias = body.as_ref().unwrap()["tools"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(alias.len(), 64);
        assert_eq!(plan.names.restore_name(alias), LONG_TOOL);
        assert_eq!(plan.applied, vec!["tool-names=1".to_string()]);
        assert_eq!(plan.header_value(), Some("tool-names=1".to_string()));
    }

    #[test]
    fn short_tool_names_leave_the_body_and_the_plan_untouched() {
        let mut body = chat_with_tool("search", false);
        let before = body.clone();
        let plan = apply_request(&mut body, &headers(&[]), true);
        assert_eq!(body, before);
        assert!(plan.names.is_empty());
        assert!(plan.applied.is_empty());
        assert!(!plan.wants_response_transform());
    }

    #[test]
    fn truncated_names_oblige_a_response_transform_on_every_shape() {
        // Non-streaming OpenAI, which is otherwise the one path that never
        // reads the response body at all.
        let mut body = chat_with_tool(LONG_TOOL, false);
        let plan = apply_request(&mut body, &headers(&[]), true);
        assert!(plan.wants_response_transform());
        assert!(matches!(
            response_transform(RouteKind::OpenAi, &plan, "p/m", false),
            Some(BodyTransform::Buffered(_))
        ));

        // Streaming, with the role repair switched off: the names alone are
        // enough to keep the rewriter engaged.
        let mut body = chat_with_tool(LONG_TOOL, true);
        let plan = apply_request(&mut body, &headers(&[]), false);
        assert!(!plan.inject_stream_role);
        assert!(matches!(
            response_transform(RouteKind::OpenAi, &plan, "p/m", true),
            Some(BodyTransform::Sse(_))
        ));
    }

    #[test]
    fn the_trail_lists_every_transform_that_fired_in_order() {
        let mut body = Some(json!({
            "model": "p/m",
            "reasoning_effort": "high",
            "messages": [
                {"role": "system", "content": "s".repeat(SYSTEM_PROMPT_MAX_CHARS + 1)},
                {"role": "user", "content": "hi"},
            ],
            "tools": [{"type": "function", "function": {"name": LONG_TOOL}}],
        }));
        let plan = apply_request(
            &mut body,
            &headers(&[
                (HEADER_SYSTEM_PROMPT_MODE, "truncate"),
                (HEADER_REASONING_STRIP, "true"),
            ]),
            true,
        );
        assert_eq!(
            plan.header_value(),
            Some("system-prompt=truncate, tool-names=1, reasoning-strip".to_string())
        );
    }
}

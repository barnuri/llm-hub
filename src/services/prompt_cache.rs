//! Prompt-cache hints added to a request body, as Claude Code does. A hint is a marker field on
//! an existing block or message, or one body field. It never changes the prompt text, the order
//! of tools, system or messages, so the cached prefix stays the same.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::schemas::prompt_shape::PromptShape;

/// Anthropic allows at most four breakpoints per request.
const MAX_BREAKPOINTS: usize = 4;
/// Messages from the end that get a breakpoint: the last two carry the moving prefix.
const TAIL_MESSAGES: usize = 2;
const ANTHROPIC_MARK: &str = "cache_control";
const COPILOT_MARK: &str = "copilot_cache_control";
const CACHE_KEY: &str = "prompt_cache_key";

/// Which hint a request's upstream reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheHint {
    /// Anthropic Messages body: `cache_control` on the last tool, system block and last messages.
    Anthropic,
    /// Chat completions to a Copilot Claude model: `copilot_cache_control` on messages.
    CopilotChat,
    /// `OpenAI`-style body: a `prompt_cache_key` that routes the request to the same cache.
    PromptKey(String),
}

impl CacheHint {
    /// Adds the hint to `body`. A body the client already marked keeps its own markers.
    pub fn apply(&self, body: &mut Value) {
        match self {
            CacheHint::Anthropic => mark_anthropic(body),
            CacheHint::CopilotChat => mark_copilot_chat(body),
            CacheHint::PromptKey(key) => set_prompt_key(body, key),
        }
    }
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// Adds llama.cpp's `cache_prompt` and a stream-scoped `id_slot` to a chat body that the client
/// did not set. The slot is picked from the caller's session and the body's stream key (system
/// prompt and tool names), so one conversation stream keeps its slot and its cached prefix. The
/// body's text is never changed. `None` when the body is not a JSON object or has both fields.
pub fn llama_slot_hints(body: &[u8], session: Option<&str>, slots: u64) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    let unset = |object: &Map<String, Value>, key: &str| object.get(key).is_none_or(Value::is_null);
    let (set_prompt, set_slot) = (unset(object, "cache_prompt"), unset(object, "id_slot"));
    if !set_prompt && !set_slot {
        return None;
    }
    let stream = PromptShape::from_body(body)
        .and_then(|shape| shape.stream_key)
        .unwrap_or_default();
    if set_prompt {
        object.insert("cache_prompt".into(), Value::Bool(true));
    }
    if set_slot {
        let slot = stream_slot(session, &stream, slots);
        object.insert("id_slot".into(), json!(slot));
    }
    serde_json::to_vec(&value).ok()
}

/// The slot of one stream: a SHA-256 over the session and the stream key, taken modulo `slots`.
/// Zero slots count as one.
pub fn stream_slot(session: Option<&str>, stream: &str, slots: u64) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(session.unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(stream.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut first = [0; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first) % slots.max(1)
}

fn set_prompt_key(body: &mut Value, key: &str) {
    if let Some(object) = body.as_object_mut()
        && object.get(CACHE_KEY).is_none_or(Value::is_null)
    {
        object.insert(CACHE_KEY.into(), Value::String(key.to_owned()));
    }
}

fn count_marks(value: &Value, mark: &str) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key(mark))
                + map.values().map(|v| count_marks(v, mark)).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(|v| count_marks(v, mark)).sum(),
        _ => 0,
    }
}

fn mark_block(block: &mut Value, mark: &str) -> bool {
    match block.as_object_mut() {
        Some(map) if !map.contains_key(mark) => {
            map.insert(mark.into(), ephemeral());
            true
        }
        _ => false,
    }
}

/// Marks the last block of `content`. A string becomes one text block with the same text.
fn mark_content(content: &mut Value, mark: &str) -> bool {
    if let Value::String(text) = content {
        if text.is_empty() {
            return false;
        }
        let mut block = Map::new();
        block.insert("type".into(), "text".into());
        block.insert("text".into(), Value::String(std::mem::take(text)));
        block.insert(mark.into(), ephemeral());
        *content = Value::Array(vec![Value::Object(block)]);
        return true;
    }
    content
        .as_array_mut()
        .and_then(|blocks| blocks.last_mut())
        .is_some_and(|block| mark_block(block, mark))
}

fn mark_anthropic(body: &mut Value) {
    // A client that placed its own breakpoints knows its prefix better than the hub.
    if count_marks(body, ANTHROPIC_MARK) > 0 {
        return;
    }
    let mut budget = MAX_BREAKPOINTS;
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if let Some(tool) = object
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
        && mark_block(tool, ANTHROPIC_MARK)
    {
        budget -= 1;
    }
    if let Some(system) = object.get_mut("system")
        && mark_content(system, ANTHROPIC_MARK)
    {
        budget -= 1;
    }
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let tail = messages.len().saturating_sub(TAIL_MESSAGES.min(budget));
    for message in &mut messages[tail..] {
        if let Some(content) = message.get_mut("content") {
            mark_content(content, ANTHROPIC_MARK);
        }
    }
}

fn mark_copilot_chat(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    // Copilot reads `copilot_cache_control` only. Carry a client's `cache_control` over to it.
    let mut carried = false;
    for message in messages.iter_mut() {
        let Some(map) = message.as_object_mut() else {
            continue;
        };
        if let Some(mark) = map.get(ANTHROPIC_MARK).cloned()
            && !map.contains_key(COPILOT_MARK)
        {
            map.insert(COPILOT_MARK.into(), mark);
            carried = true;
        }
    }
    if carried || messages.iter().any(|m| m.get(COPILOT_MARK).is_some()) {
        return;
    }
    let mut budget = MAX_BREAKPOINTS;
    if let Some(system) = messages.iter_mut().find(|m| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    }) && mark_block(system, COPILOT_MARK)
    {
        budget -= 1;
    }
    let tail = messages.len().saturating_sub(TAIL_MESSAGES.min(budget));
    for message in &mut messages[tail..] {
        mark_block(message, COPILOT_MARK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(value: &Value) -> usize {
        count_marks(value, ANTHROPIC_MARK) + count_marks(value, COPILOT_MARK)
    }

    fn without_marks(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| k.as_str() != ANTHROPIC_MARK && k.as_str() != COPILOT_MARK)
                    .map(|(k, v)| (k.clone(), without_marks(v)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(without_marks).collect()),
            other => other.clone(),
        }
    }

    fn anthropic_body() -> Value {
        json!({
            "model": "m",
            "tools": [{"name": "a"}, {"name": "b"}],
            "system": [{"type": "text", "text": "s1"}, {"type": "text", "text": "s2"}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": "x"}, {"type": "text", "text": "last"}]}
            ]
        })
    }

    #[test]
    fn anthropic_marks_tools_system_and_the_last_two_messages() {
        let mut body = anthropic_body();
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(marks(&body), 4);
        assert!(body["tools"][1].get("cache_control").is_some());
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["system"][1].get("cache_control").is_some());
        assert!(body["messages"][0]["content"].is_string());
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_some()
        );
        assert!(
            body["messages"][2]["content"][1]
                .get("cache_control")
                .is_some()
        );
        assert!(
            body["messages"][2]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn anthropic_never_changes_the_prompt() {
        let original = anthropic_body();
        let mut body = original.clone();
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(without_marks(&body), without_marks(&original));
    }

    #[test]
    fn anthropic_keeps_a_client_that_set_its_own_breakpoints() {
        let mut body = anthropic_body();
        body["system"][0]["cache_control"] = json!({"type": "ephemeral", "ttl": "1h"});
        let before = body.clone();
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn anthropic_turns_a_string_into_one_block_with_the_same_text() {
        let mut body =
            json!({"system": "be brief", "messages": [{"role": "user", "content": "hi"}]});
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(marks(&body), 2);
    }

    #[test]
    fn anthropic_skips_empty_text_and_a_body_without_the_fields() {
        let mut body = json!({"system": "", "messages": [{"role": "user", "content": ""}]});
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(marks(&body), 0);
        let mut other = json!({"model": "m"});
        CacheHint::Anthropic.apply(&mut other);
        assert_eq!(other, json!({"model": "m"}));
        let mut array = json!([1]);
        CacheHint::Anthropic.apply(&mut array);
        assert_eq!(array, json!([1]));
    }

    #[test]
    fn anthropic_skips_a_tool_or_block_that_is_not_an_object() {
        let mut body = json!({"tools": ["x"], "messages": [{"role": "user", "content": [1]}]});
        CacheHint::Anthropic.apply(&mut body);
        assert_eq!(marks(&body), 0);
    }

    #[test]
    fn copilot_chat_marks_system_and_the_last_two_messages() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": "c"}
        ]});
        let original = body.clone();
        CacheHint::CopilotChat.apply(&mut body);
        let marked: Vec<bool> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.get("copilot_cache_control").is_some())
            .collect();
        assert_eq!(marked, [true, false, true, true]);
        assert_eq!(without_marks(&body), original);
    }

    #[test]
    fn copilot_chat_carries_a_client_cache_control_over() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "a", "cache_control": {"type": "ephemeral"}},
            {"role": "user", "content": "b"}
        ]});
        CacheHint::CopilotChat.apply(&mut body);
        assert_eq!(body["messages"][0]["copilot_cache_control"], ephemeral());
        assert!(body["messages"][1].get("copilot_cache_control").is_none());
    }

    #[test]
    fn copilot_chat_keeps_existing_copilot_markers_and_handles_odd_bodies() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "a", "copilot_cache_control": {"type": "ephemeral"}},
            {"role": "user", "content": "b"}
        ]});
        let before = body.clone();
        CacheHint::CopilotChat.apply(&mut body);
        assert_eq!(body, before);
        let mut none = json!({"model": "m"});
        CacheHint::CopilotChat.apply(&mut none);
        assert_eq!(none, json!({"model": "m"}));
        let mut odd = json!({"messages": ["x", {"role": "user", "content": "q"}]});
        CacheHint::CopilotChat.apply(&mut odd);
        assert_eq!(marks(&odd), 1);
    }

    #[test]
    fn llama_hints_set_cache_prompt_and_a_stream_slot() {
        let body =
            br#"{"model":"m","system":"be terse","messages":[{"role":"user","content":"hi"}]}"#;
        let out: Value =
            serde_json::from_slice(&llama_slot_hints(body, Some("ses-1"), 4).unwrap()).unwrap();
        assert_eq!(out["cache_prompt"], true);
        let slot = out["id_slot"].as_u64().unwrap();
        assert!(slot < 4);
        let again: Value =
            serde_json::from_slice(&llama_slot_hints(body, Some("ses-1"), 4).unwrap()).unwrap();
        assert_eq!(again["id_slot"], slot, "the same stream keeps its slot");
        assert_eq!(out["messages"], json!([{"role":"user","content":"hi"}]));
    }

    #[test]
    fn llama_hints_keep_the_clients_fields_and_skip_full_bodies() {
        let body = br#"{"model":"m","messages":[],"cache_prompt":false,"id_slot":2}"#;
        assert!(llama_slot_hints(body, None, 4).is_none());
        let partial = br#"{"model":"m","messages":[],"id_slot":1}"#;
        let out: Value =
            serde_json::from_slice(&llama_slot_hints(partial, None, 4).unwrap()).unwrap();
        assert_eq!(out["cache_prompt"], true);
        assert_eq!(out["id_slot"], 1);
        assert!(llama_slot_hints(b"not json", None, 4).is_none());
        assert!(llama_slot_hints(b"[1]", None, 4).is_none());
    }

    #[test]
    fn stream_slot_stays_inside_the_slot_count() {
        assert_eq!(stream_slot(Some("a"), "k", 1), 0);
        assert_eq!(stream_slot(Some("a"), "k", 0), 0);
        assert!((0..16).all(|n| stream_slot(Some("a"), &format!("k{n}"), 3) < 3));
        assert_ne!(
            stream_slot(Some("ses-main"), "main", 8),
            stream_slot(Some("ses-other"), "main", 8)
        );
    }

    #[test]
    fn prompt_key_is_added_and_a_client_key_wins() {
        let hint = CacheHint::PromptKey("ses-1".into());
        let mut body = json!({"model": "m"});
        hint.apply(&mut body);
        assert_eq!(body["prompt_cache_key"], "ses-1");
        let mut own = json!({"prompt_cache_key": "mine"});
        hint.apply(&mut own);
        assert_eq!(own["prompt_cache_key"], "mine");
        let mut null = json!({"prompt_cache_key": null});
        hint.apply(&mut null);
        assert_eq!(null["prompt_cache_key"], "ses-1");
        let mut array = json!([]);
        hint.apply(&mut array);
        assert_eq!(array, json!([]));
    }
}

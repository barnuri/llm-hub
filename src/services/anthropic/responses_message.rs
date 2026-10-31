//! `OpenAI` Responses reply -> Anthropic Messages reply, for one whole JSON body.
//!
//! The stream translator shares the stop reason and usage mapping from here.

use serde_json::{Value, json};

use crate::consts::ANTHROPIC_THINKING_SIGNATURE;
use crate::schemas::stop_reason::reconcile_tool_use;
use crate::services::anthropic::response::{error_body, qualify_message_id};
use crate::services::tool_names::NameMap;

const INCOMPLETE_STATUS: &str = "incomplete";
const MAX_OUTPUT_TOKENS_REASON: &str = "max_output_tokens";
const UNPARSEABLE_STATUS: u16 = 502;

/// Renders a successful Responses body as an Anthropic message. `names` puts
/// back any tool name the request cut to the 64-byte limit.
#[must_use]
pub fn to_anthropic(body: &[u8], served_model: &str, names: &NameMap) -> Vec<u8> {
    let reply = match serde_json::from_slice::<Value>(body) {
        Ok(reply) => reply,
        Err(e) => {
            return error_body(
                UNPARSEABLE_STATUS,
                &format!("upstream returned a non-JSON responses body: {e}"),
            );
        }
    };
    let content: Vec<Value> = reply
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|item| content_blocks(item, names))
        .collect();
    let saw_tool_use = content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
    let message = json!({
        "id": qualify_message_id(reply.get("id").and_then(Value::as_str).unwrap_or_default()),
        "type": "message",
        "role": "assistant",
        "model": served_model,
        "content": content,
        "stop_reason": stop_reason(&reply, saw_tool_use),
        "stop_sequence": Value::Null,
        "usage": usage(&reply),
    });
    serde_json::to_vec(&message).unwrap_or_default()
}

/// `max_tokens` for a reply cut at `max_output_tokens`, else `tool_use` when
/// the message calls a tool, else `end_turn`.
pub(crate) fn stop_reason(response: &Value, saw_tool_use: bool) -> &'static str {
    let truncated = response.get("status").and_then(Value::as_str) == Some(INCOMPLETE_STATUS)
        && response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some(MAX_OUTPUT_TOKENS_REASON);
    let mapped = if truncated { "max_tokens" } else { "end_turn" };
    reconcile_tool_use(mapped, saw_tool_use)
}

/// Anthropic usage from a Responses `usage` object. Anthropic `input_tokens`
/// excludes cache reads, which go in `cache_read_input_tokens`.
pub(crate) fn usage(response: &Value) -> Value {
    let field = |pointer: &str| {
        response
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let input = field("/usage/input_tokens");
    let cached = field("/usage/input_tokens_details/cached_tokens").min(input);
    let mut usage = json!({
        "input_tokens": input - cached,
        "output_tokens": field("/usage/output_tokens"),
    });
    if cached > 0 {
        usage["cache_read_input_tokens"] = json!(cached);
    }
    usage
}

fn content_blocks(item: &Value, names: &NameMap) -> Vec<Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(|text| json!({ "type": "text", "text": text }))
            .collect(),
        Some("function_call") => vec![tool_use(item, names)],
        Some("reasoning") => reasoning_summary(item)
            .map(|thinking| {
                json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": ANTHROPIC_THINKING_SIGNATURE,
                })
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn tool_use(item: &Value, names: &NameMap) -> Value {
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input = serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    json!({
        "type": "tool_use",
        "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
        "name": names.restore_name(name),
        "input": input,
    })
}

fn reasoning_summary(item: &Value) -> Option<String> {
    let text = item
        .get("summary")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(reply: &Value) -> Value {
        let bytes = to_anthropic(
            reply.to_string().as_bytes(),
            "copilot/gpt-5.5",
            &NameMap::default(),
        );
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn text_reasoning_and_function_calls_become_content_blocks() {
        let out = translate(&json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "think"}]},
                {"type": "message", "id": "m_1", "content": [{"type": "output_text", "text": "hello"}]},
                {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"rust\"}"},
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 6}},
        }));
        assert_eq!(
            out,
            json!({
                "id": "msg_resp_1",
                "type": "message",
                "role": "assistant",
                "model": "copilot/gpt-5.5",
                "content": [
                    {"type": "thinking", "thinking": "think", "signature": ANTHROPIC_THINKING_SIGNATURE},
                    {"type": "text", "text": "hello"},
                    {"type": "tool_use", "id": "call_1", "name": "search", "input": {"q": "rust"}},
                ],
                "stop_reason": "tool_use",
                "stop_sequence": null,
                "usage": {"input_tokens": 4, "output_tokens": 4, "cache_read_input_tokens": 6},
            })
        );
    }

    #[test]
    fn a_reply_cut_at_max_output_tokens_reports_max_tokens() {
        let out = translate(&json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "par"}]}],
        }));
        assert_eq!(out["stop_reason"], "max_tokens");
        assert_eq!(out["usage"], json!({"input_tokens": 0, "output_tokens": 0}));
    }

    #[test]
    fn a_non_json_body_becomes_an_anthropic_error() {
        let bytes = to_anthropic(b"<html>", "copilot/gpt-5.5", &NameMap::default());
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["type"], "error");
    }
}

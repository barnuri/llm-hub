//! Anthropic Messages request -> `OpenAI` Responses request. Fail-open: `to_openai` already
//! rejected what this cannot carry. Thinking blocks, stop sequences and sampling are dropped.

use serde_json::{Map, Value, json};

use crate::services::anthropic::request::{
    block_type, image_url, join_text_blocks, normalize_tool_parameters, system_text,
    tool_result_text,
};
use crate::services::tool_names::{NameMap, short_name, truncate_in_body};

const LOW_EFFORT_MAX_BUDGET: u64 = 1024;
const MEDIUM_EFFORT_MAX_BUDGET: u64 = 2048;
/// The smallest `max_output_tokens` the Responses API accepts; it rejects 8 with a 400.
const MIN_MAX_OUTPUT_TOKENS: u64 = 16;

/// Raises a `max_tokens` below the Responses minimum to that minimum. A non-integer is kept as sent.
pub(crate) fn clamp_max_output_tokens(max_tokens: &Value) -> Value {
    max_tokens.as_u64().map_or_else(
        || max_tokens.clone(),
        |tokens| json!(tokens.max(MIN_MAX_OUTPUT_TOKENS)),
    )
}

/// Translates an Anthropic Messages body into a Responses body, and returns
/// the mapping that undoes any tool name cut to the 64-byte limit.
#[must_use]
pub fn to_responses(body: &Value) -> (Value, NameMap) {
    let mut out = Map::new();
    if let Some(model) = body.get("model") {
        out.insert("model".into(), model.clone());
    }
    if let Some(text) = body.get("system").and_then(system_text) {
        out.insert("instructions".into(), Value::String(text));
    }
    let mut input = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push_message(message, &mut input);
    }
    out.insert("input".into(), Value::Array(input));
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools.iter().filter_map(function_tool).collect::<Vec<_>>())
        .unwrap_or_default();
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = body.get("tool_choice") {
        insert_tool_choice(choice, &mut out);
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        out.insert(
            "max_output_tokens".into(),
            clamp_max_output_tokens(max_tokens),
        );
    }
    if let Some(stream) = body.get("stream") {
        out.insert("stream".into(), stream.clone());
    }
    if let Some(effort) = reasoning_effort(body) {
        out.insert("reasoning".into(), json!({ "effort": effort }));
    }
    out.insert("store".into(), Value::Bool(false));
    let names = truncate_in_body(&mut out);
    (Value::Object(out), names)
}

fn push_message(message: &Value, input: &mut Vec<Value>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let blocks = match message.get("content") {
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => Vec::new(),
    };
    match role {
        "assistant" => push_assistant(&blocks, input),
        "system" => push_text_message("developer", &join_text_blocks(&blocks, "\n\n"), input),
        _ => push_user(role, &blocks, input),
    }
}

/// Text runs become `output_text` messages and each `tool_use` a
/// `function_call`, in block order.
fn push_assistant(blocks: &[Value], input: &mut Vec<Value>) {
    let mut text: Vec<&str> = Vec::new();
    for block in blocks {
        match block_type(block) {
            "text" => text.extend(block.get("text").and_then(Value::as_str)),
            "tool_use" => {
                flush_assistant_text(&mut text, input);
                input.push(function_call(block));
            }
            _ => {}
        }
    }
    flush_assistant_text(&mut text, input);
}

fn flush_assistant_text(text: &mut Vec<&str>, input: &mut Vec<Value>) {
    let joined = text.join("\n\n");
    text.clear();
    if joined.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": joined }],
    }));
}

/// `tool_result` blocks become `function_call_output` items ahead of the user
/// message that carries the remaining text and image blocks.
fn push_user(role: &str, blocks: &[Value], input: &mut Vec<Value>) {
    let mut parts = Vec::new();
    for block in blocks {
        match block_type(block) {
            "tool_result" => input.push(json!({
                "type": "function_call_output",
                "call_id": string_field(block, "tool_use_id"),
                "output": tool_result_text(block),
            })),
            "text" => {
                let text = string_field(block, "text");
                if !text.is_empty() {
                    parts.push(json!({ "type": "input_text", "text": text }));
                }
            }
            "image" => {
                if let Ok(url) = image_url(block) {
                    parts.push(json!({ "type": "input_image", "image_url": url }));
                }
            }
            _ => {}
        }
    }
    if !parts.is_empty() {
        input.push(json!({ "type": "message", "role": role, "content": parts }));
    }
}

fn push_text_message(role: &str, text: &str, input: &mut Vec<Value>) {
    if text.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": role,
        "content": [{ "type": "input_text", "text": text }],
    }));
}

fn function_call(block: &Value) -> Value {
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    let name = string_field(block, "name");
    json!({
        "type": "function_call",
        "call_id": string_field(block, "id"),
        "name": short_name(name),
        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string()),
    })
}

/// Only custom tools carry an `input_schema`; Anthropic server tools are skipped.
/// `strict` is off because Responses defaults it on, and Anthropic schemas
/// rarely meet strict mode's rules.
fn function_tool(tool: &Value) -> Option<Value> {
    let schema = tool.get("input_schema")?;
    let mut function = Map::new();
    function.insert("type".into(), json!("function"));
    function.insert("name".into(), json!(string_field(tool, "name")));
    if let Some(description) = tool.get("description") {
        function.insert("description".into(), description.clone());
    }
    function.insert("parameters".into(), normalize_tool_parameters(schema));
    function.insert("strict".into(), Value::Bool(false));
    Some(Value::Object(function))
}

fn insert_tool_choice(choice: &Value, out: &mut Map<String, Value>) {
    let translated = match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(json!("auto")),
        Some("any") => Some(json!("required")),
        Some("none") => Some(json!("none")),
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "name": name })),
        _ => None,
    };
    if let Some(translated) = translated {
        out.insert("tool_choice".into(), translated);
    }
    if choice
        .get("disable_parallel_tool_use")
        .and_then(Value::as_bool)
        == Some(true)
    {
        out.insert("parallel_tool_calls".into(), Value::Bool(false));
    }
}

/// `output_config.effort` wins. Otherwise an enabled `thinking` budget picks
/// the effort tier.
pub(crate) fn reasoning_effort(body: &Value) -> Option<&'static str> {
    let effort = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str);
    if let Some(effort) = effort {
        return match effort {
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" | "max" => Some("high"),
            _ => None,
        };
    }
    let thinking = body.get("thinking")?;
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return None;
    }
    let budget = thinking.get("budget_tokens").and_then(Value::as_u64)?;
    Some(if budget <= LOW_EFFORT_MAX_BUDGET {
        "low"
    } else if budget <= MEDIUM_EFFORT_MAX_BUDGET {
        "medium"
    } else {
        "high"
    })
}

fn string_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: &Value) -> Value {
        to_responses(body).0
    }

    #[test]
    fn system_messages_and_scalars_map_to_responses_fields() {
        let out = translate(&json!({
            "model": "copilot/gpt-5.5",
            "system": [{"type": "text", "text": "one"}, {"type": "text", "text": "two"}],
            "max_tokens": 512,
            "stream": true,
            "temperature": 0.2,
            "stop_sequences": ["END"],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(
            out,
            json!({
                "model": "copilot/gpt-5.5",
                "instructions": "one\n\ntwo",
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}],
                "max_output_tokens": 512,
                "stream": true,
                "store": false,
            })
        );
    }

    #[test]
    fn max_output_tokens_below_the_responses_minimum_is_raised_to_it() {
        let max_output = |tokens: u64| {
            translate(&json!({"max_tokens": tokens, "messages": []}))["max_output_tokens"].clone()
        };
        assert_eq!(max_output(8), json!(16));
        assert_eq!(max_output(16), json!(16));
        assert_eq!(max_output(512), json!(512));
    }

    #[test]
    fn tool_use_and_tool_result_become_function_call_items_in_order() {
        let out = translate(&json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "s"},
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "call_1", "name": "weather", "input": {"city": "Oslo"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1",
                     "content": [{"type": "text", "text": "sunny"}]},
                    {"type": "text", "text": "thanks"},
                ]},
            ],
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
                {"type": "function_call", "call_id": "call_1", "name": "weather", "arguments": "{\"city\":\"Oslo\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]},
            ])
        );
    }

    #[test]
    fn images_and_mid_conversation_system_messages_are_carried() {
        let out = translate(&json!({
            "messages": [
                {"role": "system", "content": "mode changed"},
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AA"}},
                ]},
            ],
        }));
        assert_eq!(
            out["input"],
            json!([
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "mode changed"}]},
                {"type": "message", "role": "user",
                 "content": [{"type": "input_image", "image_url": "data:image/jpeg;base64,AA"}]},
            ])
        );
    }

    #[test]
    fn custom_tools_become_non_strict_function_tools_and_server_tools_are_dropped() {
        let out = translate(&json!({
            "messages": [],
            "tools": [
                {"name": "search", "description": "find", "input_schema": {"type": "object"}},
                {"type": "web_search_20250305", "name": "web_search"},
            ],
            "tool_choice": {"type": "tool", "name": "search", "disable_parallel_tool_use": true},
        }));
        assert_eq!(
            out["tools"],
            json!([{"type": "function", "name": "search", "description": "find",
                    "parameters": {"type": "object", "properties": {}}, "strict": false}])
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": "search"})
        );
        assert_eq!(out["parallel_tool_calls"], false);
    }

    #[test]
    fn tool_choice_kinds_map_to_responses_values() {
        let choice = |kind: &str| {
            translate(&json!({"messages": [], "tool_choice": {"type": kind}}))["tool_choice"]
                .clone()
        };
        assert_eq!(choice("auto"), "auto");
        assert_eq!(choice("any"), "required");
        assert_eq!(choice("none"), "none");
    }

    #[test]
    fn long_tool_names_are_cut_everywhere_and_restorable() {
        let long = "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";
        let (out, names) = to_responses(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "c", "name": long, "input": {}},
            ]}],
            "tools": [{"name": long, "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": long},
        }));
        let alias = out["tools"][0]["name"].as_str().unwrap();
        assert_eq!(alias.len(), 64);
        assert_eq!(out["input"][0]["name"], alias);
        assert_eq!(out["tool_choice"]["name"], alias);
        assert_eq!(names.restore_name(alias), long);
    }

    #[test]
    fn effort_comes_from_output_config_then_from_the_thinking_budget() {
        let effort = |body: Value| translate(&body)["reasoning"]["effort"].clone();
        assert_eq!(
            effort(json!({"messages": [], "output_config": {"effort": "max"}})),
            "high"
        );
        assert_eq!(
            effort(json!({"messages": [], "output_config": {"effort": "low"},
                                 "thinking": {"type": "enabled", "budget_tokens": 32000}})),
            "low"
        );
        let budget = |tokens: u64| {
            effort(json!({"messages": [],
            "thinking": {"type": "enabled", "budget_tokens": tokens}}))
        };
        assert_eq!(budget(1024), "low");
        assert_eq!(budget(2048), "medium");
        assert_eq!(budget(31999), "high");
        assert!(
            translate(&json!({"messages": [], "thinking": {"type": "disabled"}}))
                .get("reasoning")
                .is_none()
        );
    }
}

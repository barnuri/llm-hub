//! Anthropic Messages request -> `OpenAI` chat-completions request.
//!
//! Value-based, like the rest of the proxy path: the wire shape gains fields
//! every few months and a typed mirror would only add churn. Translation is
//! fail-open — an unrepresentable block is dropped, not rejected — with three
//! deliberate exceptions that would otherwise produce a silently wrong request:
//! a body that is not an object, a missing `messages` array, and an image
//! source the `OpenAI` shape cannot carry.
//!
//! Dropped on purpose, because `OpenAI` chat-completions has no equivalent:
//! `redacted_thinking` content blocks, the top-level `thinking` parameter,
//! `top_k`, and Anthropic server tools (`web_search_*`, `computer_*` — anything
//! without an `input_schema`). `thinking` blocks are preserved as
//! `reasoning_content` for local chat templates that support multi-turn thinking.

use serde_json::{Map, Value, json};

use crate::consts::ANTHROPIC_THINKING_SIGNATURE;
use crate::schemas::app_error::AppError;

/// Translates an Anthropic Messages body into the `OpenAI` chat-completions
/// shape. `model` is copied verbatim: it carries the same `<profile>/<model>`
/// meaning on both routes, so the caller's fallback chain still works.
///
/// # Errors
/// `AppError::BadRequest` when the body is not an object, when `messages` is
/// missing or not an array, when a message is not an object, or when an image
/// block uses a source type other than `base64` or `url`.
pub fn to_openai(body: Option<&Value>) -> Result<Value, AppError> {
    let Some(Value::Object(src)) = body else {
        return Err(AppError::BadRequest(
            "anthropic request body must be a JSON object".to_string(),
        ));
    };
    let Some(Value::Array(messages)) = src.get("messages") else {
        return Err(AppError::BadRequest(
            "anthropic request body must contain a \"messages\" array".to_string(),
        ));
    };

    let mut out_messages: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = src.get("system")
        && let Some(text) = system_text(system)
    {
        out_messages.push(json!({ "role": "system", "content": text }));
    }
    for message in messages {
        translate_message(message, &mut out_messages)?;
    }

    let mut out = Map::new();
    if let Some(model) = src.get("model") {
        out.insert("model".into(), model.clone());
    }
    out.insert("messages".into(), Value::Array(out_messages));
    copy_scalars(src, &mut out);

    if let Some(tools) = src.get("tools").and_then(Value::as_array) {
        let converted = translate_tools(tools);
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if let Some(choice) = src.get("tool_choice").and_then(translate_tool_choice) {
        out.insert("tool_choice".into(), choice);
    }
    Ok(Value::Object(out))
}

/// `system` is either a string or a block list; non-text blocks are skipped.
pub(crate) fn system_text(system: &Value) -> Option<String> {
    let text = match system {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => join_text_blocks(blocks, "\n\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn translate_message(message: &Value, out: &mut Vec<Value>) -> Result<(), AppError> {
    let Some(obj) = message.as_object() else {
        return Err(AppError::BadRequest(
            "each anthropic message must be an object".to_string(),
        ));
    };
    let role = obj.get("role").and_then(Value::as_str).unwrap_or("user");
    if role == "system" {
        out.push(mid_conversation_system(obj.get("content")));
        return Ok(());
    }
    match obj.get("content") {
        Some(Value::String(text)) => out.push(json!({ "role": role, "content": text })),
        Some(Value::Array(blocks)) if role == "assistant" => push_assistant(blocks, out),
        Some(Value::Array(blocks)) => push_user(role, blocks, out)?,
        _ => out.push(json!({ "role": role, "content": "" })),
    }
    Ok(())
}

/// Claude Code can put `role: "system"` messages inside `messages`. Qwen and
/// other chat templates reject a system message that is not first ("System
/// message must be at the beginning"), which 400s the whole turn. Sending it as
/// a user-side reminder keeps the instruction and the leading prompt prefix,
/// so the upstream prompt cache still hits.
fn mid_conversation_system(content: Option<&Value>) -> Value {
    let text = match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => join_text_blocks(blocks, "\n\n"),
        _ => String::new(),
    };
    json!({
        "role": "user",
        "content": format!("<system-reminder>\n{text}\n</system-reminder>"),
    })
}

/// One assistant message: joined text (or `null`) plus any `tool_use` blocks
/// hoisted into `tool_calls`, in block order.
fn push_assistant(blocks: &[Value], out: &mut Vec<Value>) {
    let text = join_text_blocks(blocks, "\n\n");
    let thinking = join_thinking_blocks(blocks);
    let tool_calls: Vec<Value> = blocks
        .iter()
        .filter(|block| block_type(block) == "tool_use")
        .map(tool_call)
        .collect();

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if !thinking.is_empty() {
        message.insert("reasoning_content".into(), Value::String(thinking));
    }
    out.push(Value::Object(message));
}

/// Anthropic carries `tool_result` blocks inside a *user* message; `OpenAI` needs
/// them as standalone `tool` messages. Emit those first, in block order, then a
/// user message with whatever blocks remain — omitted entirely when none do.
fn push_user(role: &str, blocks: &[Value], out: &mut Vec<Value>) -> Result<(), AppError> {
    let mut parts: Vec<Value> = Vec::new();
    let mut tool_messages: Vec<Value> = Vec::new();
    for block in blocks {
        match block_type(block) {
            "tool_result" => tool_messages.push(json!({
                "role": "tool",
                "tool_call_id": string_field(block, "tool_use_id"),
                "content": tool_result_text(block),
            })),
            "text" => parts.push(json!({ "type": "text", "text": string_field(block, "text") })),
            "image" => parts.push(image_part(block)?),
            _ => {}
        }
    }
    out.append(&mut tool_messages);
    if !parts.is_empty() {
        out.push(json!({ "role": role, "content": parts }));
    }
    Ok(())
}

fn tool_call(block: &Value) -> Value {
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    json!({
        "id": string_field(block, "id"),
        "type": "function",
        "function": {
            "name": string_field(block, "name"),
            "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string()),
        }
    })
}

/// `tool_result.content` is a string or a block list; only text survives.
pub(crate) fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => join_text_blocks(blocks, "\n"),
        _ => String::new(),
    }
}

fn image_part(block: &Value) -> Result<Value, AppError> {
    Ok(json!({ "type": "image_url", "image_url": { "url": image_url(block)? } }))
}

/// The image of an `image` block as a URL: a `data:` URL for a base64 source.
///
/// # Errors
/// `AppError::BadRequest` when the block has no `source` object, or when the
/// source type is neither `base64` nor `url`.
pub(crate) fn image_url(block: &Value) -> Result<String, AppError> {
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return Err(AppError::BadRequest(
            "image block requires a \"source\" object".to_string(),
        ));
    };
    let url = match source
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("data:{media_type};base64,{data}")
        }
        "url" => source
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported image source type: {other} (expected base64 or url)"
            )));
        }
    };
    Ok(url)
}

/// Removes server tools (`web_search_*`, `computer_*`, ...) from an Anthropic body
/// and drops `tools` when nothing is left. Copilot's native `/v1/messages` rejects
/// them with a 400, so a native attempt sends the body without them.
pub(crate) fn drop_server_tools(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    tools.retain(|tool| tool.get("input_schema").is_some());
    if tools.is_empty() {
        object.remove("tools");
    }
}

/// Copilot's native `/v1/messages` rejects `thinking.type: enabled` on Haiku 5.5 and
/// wants `adaptive` with `output_config.effort`. An enabled budget becomes the effort
/// tier the Responses bridge picks for the same budget. An effort the client set wins.
pub(crate) fn adaptive_thinking(body: &mut Value) {
    if body.pointer("/thinking/type").and_then(Value::as_str) != Some("enabled") {
        return;
    }
    let effort = super::responses_request::reasoning_effort(body);
    body["thinking"] = json!({"type": "adaptive"});
    if let Some(effort) = effort
        && body.pointer("/output_config/effort").is_none()
    {
        body["output_config"]["effort"] = json!(effort);
    }
}

/// Removes `thinking` blocks signed with the hub's placeholder. The hub writes that
/// signature onto reasoning from a non-Claude reply, and Copilot's native `/v1/messages`
/// rejects it as an invalid signature. A message left with no content is dropped too.
pub(crate) fn drop_placeholder_thinking(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let is_placeholder = |block: &Value| {
        block_type(block) == "thinking"
            && block.get("signature").and_then(Value::as_str) == Some(ANTHROPIC_THINKING_SIGNATURE)
    };
    for message in messages.iter_mut() {
        if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
            blocks.retain(|block| !is_placeholder(block));
        }
    }
    messages.retain(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_none_or(|blocks| !blocks.is_empty())
    });
}

/// Custom tools carry an `input_schema`; server tools (`web_search_*`,
/// `computer_*`, ...) do not and have no `OpenAI` equivalent, so they are skipped.
fn translate_tools(tools: &[Value]) -> Vec<Value> {
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let Some(schema) = obj.get("input_schema") else {
            let label = obj
                .get("name")
                .or_else(|| obj.get("type"))
                .and_then(|value| value.as_str())
                .unwrap_or("<unnamed>");
            tracing::warn!("skipping anthropic tool without input_schema: {label}");
            continue;
        };
        let mut function = Map::new();
        function.insert("name".into(), json!(string_field(tool, "name")));
        if let Some(description) = obj.get("description") {
            function.insert("description".into(), description.clone());
        }
        function.insert("parameters".into(), normalize_tool_parameters(schema));
        out.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    out
}

/// Fills in the JSON-Schema fields an Anthropic `input_schema` may omit.
///
/// Anthropic and `OpenAI` both accept a bare `{"type": "object"}`, but stricter
/// `OpenAI`-compatible servers (LM Studio) reject a `parameters` object without
/// `properties`. Only the top level is normalized — nested schemas are the
/// caller's shape and are copied through untouched.
pub(crate) fn normalize_tool_parameters(schema: &Value) -> Value {
    let Some(fields) = schema.as_object() else {
        return json!({ "type": "object", "properties": {} });
    };
    let mut parameters = fields.clone();
    parameters.entry("type").or_insert_with(|| json!("object"));
    parameters.entry("properties").or_insert_with(|| json!({}));
    Value::Object(parameters)
}

fn translate_tool_choice(choice: &Value) -> Option<Value> {
    let obj = choice.as_object()?;
    match obj.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => obj
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "function": { "name": name } })),
        _ => None,
    }
}

/// Parameters that carry over unchanged, plus the two that are renamed.
/// `top_k` and `thinking` are intentionally absent — see the module comment.
fn copy_scalars(src: &Map<String, Value>, out: &mut Map<String, Value>) {
    for key in ["max_tokens", "temperature", "top_p", "stream"] {
        if let Some(value) = src.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(stop) = src.get("stop_sequences") {
        out.insert("stop".into(), stop.clone());
    }
    if let Some(user) = src.get("metadata").and_then(|m| m.get("user_id")) {
        out.insert("user".into(), user.clone());
    }
}

pub(crate) fn join_text_blocks(blocks: &[Value], separator: &str) -> String {
    blocks
        .iter()
        .filter(|block| block_type(block) == "text")
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join(separator)
}

/// Only the reasoning text crosses over: the block's `signature` (our own
/// `ANTHROPIC_THINKING_SIGNATURE` placeholder, or a real one from an earlier
/// Anthropic turn) has no `OpenAI` meaning and is never forwarded.
fn join_thinking_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block_type(block) == "thinking")
        .filter_map(|block| block.get("thinking").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<&str>>()
        .join("\n\n")
}

pub(crate) fn block_type(block: &Value) -> &str {
    block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(body: &Value) -> Value {
        to_openai(Some(body)).expect("translates")
    }

    fn messages(body: &Value) -> &Vec<Value> {
        body["messages"].as_array().unwrap()
    }

    #[test]
    fn system_string_becomes_first_system_message() {
        let out = translate(&json!({
            "model": "p/m", "system": "be terse",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        assert_eq!(
            messages(&out)[0],
            json!({"role": "system", "content": "be terse"})
        );
        assert_eq!(messages(&out)[1], json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn mid_conversation_system_becomes_user_reminder() {
        let out = translate(&json!({
            "model": "p/m", "system": "top",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [{"type": "text", "text": "mode changed"}]},
                {"role": "system", "content": "plain"}
            ]
        }));
        let msgs = messages(&out);
        assert_eq!(msgs[0], json!({"role": "system", "content": "top"}));
        assert_eq!(
            msgs[2],
            json!({"role": "user", "content": "<system-reminder>\nmode changed\n</system-reminder>"})
        );
        assert_eq!(
            msgs[3],
            json!({"role": "user", "content": "<system-reminder>\nplain\n</system-reminder>"})
        );
        assert_eq!(msgs.iter().filter(|m| m["role"] == "system").count(), 1);
    }

    #[test]
    fn system_blocks_join_with_blank_line() {
        let out = translate(&json!({
            "system": [
                {"type": "text", "text": "one"},
                {"type": "image", "source": {"type": "url", "url": "http://x"}},
                {"type": "text", "text": "two"}
            ],
            "messages": []
        }));
        assert_eq!(messages(&out)[0]["content"], "one\n\ntwo");
    }

    #[test]
    fn user_text_blocks_become_content_parts() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
        }));
        assert_eq!(
            messages(&out)[0]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
    }

    #[test]
    fn plain_string_content_passes_through() {
        let out = translate(&json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(messages(&out)[0]["content"], "hi");
    }

    #[test]
    fn base64_image_becomes_data_url() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}
            ]}]
        }));
        assert_eq!(
            messages(&out)[0]["content"][0]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
    }

    #[test]
    fn url_image_becomes_image_url() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}}
            ]}]
        }));
        assert_eq!(
            messages(&out)[0]["content"][0],
            json!({"type": "image_url", "image_url": {"url": "https://example.com/a.png"}})
        );
    }

    #[test]
    fn unsupported_image_source_is_bad_request() {
        let body = json!({
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "file", "file_id": "f1"}}
            ]}]
        });
        let error = to_openai(Some(&body)).unwrap_err();
        assert!(matches!(error, AppError::BadRequest(ref m) if m.contains("file")));
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_string_arguments() {
        let out = translate(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "text", "text": "let me look"},
                {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {"q": "rust"}}
            ]}]
        }));
        let message = &messages(&out)[0];
        assert_eq!(message["content"], "let me look");
        assert_eq!(message["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"rust\"}"
        );
    }

    #[test]
    fn assistant_tool_use_without_text_has_null_content() {
        let out = translate(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "n", "input": {}}
            ]}]
        }));
        assert_eq!(messages(&out)[0]["content"], Value::Null);
    }

    #[test]
    fn user_tool_result_hoists_to_tool_role_message_before_remaining_text() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "42"},
                {"type": "text", "text": "and now?"}
            ]}]
        }));
        assert_eq!(
            messages(&out)[0],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "42"})
        );
        assert_eq!(messages(&out)[1]["role"], "user");
        assert_eq!(
            messages(&out)[1]["content"],
            json!([{"type": "text", "text": "and now?"}])
        );
    }

    #[test]
    fn tool_result_block_content_joins_text_blocks() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [
                    {"type": "text", "text": "a"}, {"type": "text", "text": "b"}
                ]}
            ]}]
        }));
        assert_eq!(messages(&out)[0]["content"], "a\nb");
    }

    #[test]
    fn tool_result_only_user_message_emits_no_user_message() {
        let out = translate(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "ok"}
            ]}]
        }));
        assert_eq!(messages(&out).len(), 1);
        assert_eq!(messages(&out)[0]["role"], "tool");
    }

    #[test]
    fn tools_become_openai_function_tools_with_parameters() {
        let out = translate(&json!({
            "messages": [],
            "tools": [{
                "name": "search", "description": "find things",
                "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}
            }]
        }));
        assert_eq!(
            out["tools"][0],
            json!({"type": "function", "function": {
                "name": "search", "description": "find things",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
            }})
        );
    }

    #[test]
    fn server_tool_entries_are_skipped() {
        let out = translate(&json!({
            "messages": [],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "keep", "input_schema": {"type": "object"}}
            ]
        }));
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["tools"][0]["function"]["name"], "keep");
    }

    #[test]
    fn adaptive_thinking_turns_an_enabled_budget_into_an_effort_tier() {
        let mut body = json!({"thinking": {"type": "enabled", "budget_tokens": 4096}});
        adaptive_thinking(&mut body);
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"]["effort"], "high");

        let mut kept = json!({
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "output_config": {"effort": "medium"}
        });
        adaptive_thinking(&mut kept);
        assert_eq!(kept["output_config"]["effort"], "medium");

        let mut untouched = json!({"thinking": {"type": "adaptive"}});
        adaptive_thinking(&mut untouched);
        assert_eq!(untouched, json!({"thinking": {"type": "adaptive"}}));
    }

    #[test]
    fn drop_server_tools_keeps_custom_tools_and_drops_empty_list() {
        let mut body = json!({
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "keep", "input_schema": {"type": "object"}}
            ]
        });
        drop_server_tools(&mut body);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(body["tools"][0]["name"], "keep");

        let mut only_server =
            json!({"tools": [{"type": "web_search_20250305", "name": "web_search"}]});
        drop_server_tools(&mut only_server);
        assert!(only_server.get("tools").is_none());
    }

    #[test]
    fn tool_schema_without_properties_gets_empty_properties() {
        let out = translate(&json!({
            "messages": [],
            "tools": [{"name": "no_args", "input_schema": {"type": "object"}}]
        }));
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn tool_schema_without_type_keeps_existing_properties() {
        let out = translate(&json!({
            "messages": [],
            "tools": [{
                "name": "typeless",
                "input_schema": {"properties": {"q": {"type": "string"}}, "required": ["q"]}
            }]
        }));
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]})
        );
    }

    #[test]
    fn non_object_tool_schema_becomes_empty_object_schema() {
        let out = translate(&json!({
            "messages": [],
            "tools": [{"name": "broken", "input_schema": "nonsense"}]
        }));
        assert_eq!(
            out["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn tool_choice_auto_any_none_and_named_map_correctly() {
        let choice = |value: Value| {
            translate(&json!({"messages": [], "tool_choice": value}))["tool_choice"].clone()
        };
        assert_eq!(choice(json!({"type": "auto"})), json!("auto"));
        assert_eq!(choice(json!({"type": "any"})), json!("required"));
        assert_eq!(choice(json!({"type": "none"})), json!("none"));
        assert_eq!(
            choice(json!({"type": "tool", "name": "search"})),
            json!({"type": "function", "function": {"name": "search"}})
        );
    }

    #[test]
    fn stop_sequences_becomes_stop_and_top_k_is_dropped() {
        let out = translate(&json!({
            "messages": [], "stop_sequences": ["END"], "top_k": 5,
            "max_tokens": 128, "temperature": 0.2, "metadata": {"user_id": "u1"}
        }));
        assert_eq!(out["stop"], json!(["END"]));
        assert_eq!(out["max_tokens"], 128);
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["user"], "u1");
        assert!(out.get("top_k").is_none());
    }

    #[test]
    fn thinking_blocks_become_reasoning_content_and_redacted_thinking_is_dropped() {
        let out = translate(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "redacted_thinking", "data": "xx"},
                {"type": "text", "text": "answer"}
            ]}],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        }));
        assert_eq!(messages(&out)[0]["content"], "answer");
        assert_eq!(messages(&out)[0]["reasoning_content"], "hmm");
        assert!(out.get("thinking").is_none());
    }

    #[test]
    fn placeholder_signature_is_not_forwarded_with_reasoning_content() {
        let out = translate(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": crate::consts::ANTHROPIC_THINKING_SIGNATURE},
                {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
            ]}]
        }));
        let message = &messages(&out)[0];
        assert_eq!(message["reasoning_content"], "plan");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "bash");
        assert!(
            !out.to_string()
                .contains(crate::consts::ANTHROPIC_THINKING_SIGNATURE)
        );
    }

    #[test]
    fn placeholder_thinking_is_dropped_from_native_body_and_real_signatures_are_kept() {
        let mut body = json!({"messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "gpt reasoning", "signature": crate::consts::ANTHROPIC_THINKING_SIGNATURE},
                {"type": "text", "text": "answer"}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "only placeholder", "signature": crate::consts::ANTHROPIC_THINKING_SIGNATURE}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "claude", "signature": "EqoBreal"},
                {"type": "text", "text": "ok"}
            ]},
            {"role": "user", "content": "plain string stays"}
        ]});
        drop_placeholder_thinking(&mut body);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0]["content"],
            json!([{"type": "text", "text": "answer"}])
        );
        assert_eq!(messages[1]["content"][0]["signature"], "EqoBreal");
        assert_eq!(messages[2]["content"], "plain string stays");
        assert!(
            !body
                .to_string()
                .contains(crate::consts::ANTHROPIC_THINKING_SIGNATURE)
        );
    }

    #[test]
    fn model_is_preserved_verbatim_for_routing() {
        let out = translate(&json!({"model": "groq/meta-llama/L4", "messages": []}));
        assert_eq!(out["model"], "groq/meta-llama/L4");
    }

    #[test]
    fn missing_messages_is_bad_request() {
        let error = to_openai(Some(&json!({"model": "p/m"}))).unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)));
    }

    #[test]
    fn non_object_body_is_bad_request() {
        assert!(matches!(
            to_openai(None).unwrap_err(),
            AppError::BadRequest(_)
        ));
        assert!(matches!(
            to_openai(Some(&json!([1, 2]))).unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    #[test]
    fn non_object_message_is_bad_request() {
        let error = to_openai(Some(&json!({"messages": ["hi"]}))).unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)));
    }
}

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
//! `thinking` / `redacted_thinking` content blocks, the top-level `thinking`
//! parameter, `top_k`, and Anthropic server tools (`web_search_*`, `computer_*`
//! — anything without an `input_schema`).

use serde_json::{Map, Value, json};

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
fn system_text(system: &Value) -> Option<String> {
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
    match obj.get("content") {
        Some(Value::String(text)) => out.push(json!({ "role": role, "content": text })),
        Some(Value::Array(blocks)) if role == "assistant" => push_assistant(blocks, out),
        Some(Value::Array(blocks)) => push_user(role, blocks, out)?,
        _ => out.push(json!({ "role": role, "content": "" })),
    }
    Ok(())
}

/// One assistant message: joined text (or `null`) plus any `tool_use` blocks
/// hoisted into `tool_calls`, in block order.
fn push_assistant(blocks: &[Value], out: &mut Vec<Value>) {
    let text = join_text_blocks(blocks, "\n\n");
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
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => join_text_blocks(blocks, "\n"),
        _ => String::new(),
    }
}

fn image_part(block: &Value) -> Result<Value, AppError> {
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
    Ok(json!({ "type": "image_url", "image_url": { "url": url } }))
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
        function.insert("parameters".into(), schema.clone());
        out.push(json!({ "type": "function", "function": Value::Object(function) }));
    }
    out
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

fn join_text_blocks(blocks: &[Value], separator: &str) -> String {
    blocks
        .iter()
        .filter(|block| block_type(block) == "text")
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join(separator)
}

fn block_type(block: &Value) -> &str {
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
    fn thinking_and_redacted_thinking_blocks_are_dropped() {
        let out = translate(&json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "redacted_thinking", "data": "xx"},
                {"type": "text", "text": "answer"}
            ]}],
            "thinking": {"type": "enabled", "budget_tokens": 1024}
        }));
        assert_eq!(messages(&out)[0]["content"], "answer");
        assert!(out.get("thinking").is_none());
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

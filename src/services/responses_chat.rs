//! `OpenAI` chat-completions request -> Responses request, and the Responses
//! reply (whole body or SSE) -> chat-completions shape. It lets a client that
//! calls `/chat/completions` reach a Copilot model that serves only `/responses`.
//!
//! A feature whose meaning would change in translation (`n > 1`, a
//! `response_format`, logprobs, a non-function tool, a non-text content part) is
//! refused with an error, never dropped. Sampling and stop parameters are dropped,
//! as the Anthropic bridge drops them.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::services::anthropic::request::normalize_tool_parameters;
use crate::services::anthropic::responses_message::stop_reason;
use crate::services::anthropic::responses_request::clamp_max_output_tokens;
use crate::services::sse::{SseFrames, parse_data_frame};
use crate::services::tool_names::{NameMap, short_name, truncate_in_body};

const UNSUPPORTED_FIELDS: &[&str] = &[
    "response_format",
    "functions",
    "function_call",
    "audio",
    "modalities",
    "prediction",
    "web_search_options",
];

/// Translates a chat-completions body into a Responses body, and returns the
/// mapping that undoes any tool name cut to the 64-byte limit. `Err` names the
/// first feature the Responses API cannot carry with the same meaning.
pub fn chat_to_responses(body: &Value) -> Result<(Value, NameMap), String> {
    refuse_unsupported(body)?;
    let mut out = Map::new();
    if let Some(model) = body.get("model") {
        out.insert("model".into(), model.clone());
    }
    let mut input = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push_message(message, &mut input)?;
    }
    out.insert("input".into(), Value::Array(input));
    let tools = function_tools(body)?;
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = body.get("tool_choice") {
        out.insert("tool_choice".into(), tool_choice(choice)?);
    }
    if let Some(parallel) = body.get("parallel_tool_calls").and_then(Value::as_bool) {
        out.insert("parallel_tool_calls".into(), Value::Bool(parallel));
    }
    if let Some(max_tokens) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
    {
        out.insert(
            "max_output_tokens".into(),
            clamp_max_output_tokens(max_tokens),
        );
    }
    if let Some(stream) = body.get("stream") {
        out.insert("stream".into(), stream.clone());
    }
    if let Some(effort) = body.get("reasoning_effort") {
        out.insert(
            "reasoning".into(),
            json!({ "effort": reasoning_effort(effort)? }),
        );
    }
    out.insert("store".into(), Value::Bool(false));
    let names = truncate_in_body(&mut out);
    Ok((Value::Object(out), names))
}

fn refuse_unsupported(body: &Value) -> Result<(), String> {
    if body
        .get("n")
        .and_then(Value::as_u64)
        .is_some_and(|n| n != 1)
    {
        return Err("n other than 1 is not supported on a Responses-only model".into());
    }
    if body.get("logprobs").and_then(Value::as_bool) == Some(true) {
        return Err("logprobs are not supported on a Responses-only model".into());
    }
    for field in UNSUPPORTED_FIELDS {
        if body.get(*field).is_some_and(|value| !value.is_null()) {
            return Err(format!(
                "{field} is not supported on a Responses-only model"
            ));
        }
    }
    Ok(())
}

fn push_message(message: &Value, input: &mut Vec<Value>) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = message.get("content").unwrap_or(&Value::Null);
    match role {
        "system" | "developer" => {
            push_text_message("developer", "input_text", &text_of(content)?, input);
            Ok(())
        }
        "user" => push_user(content, input),
        "assistant" => push_assistant(message, content, input),
        "tool" => {
            input.push(json!({
                "type": "function_call_output",
                "call_id": string_field(message, "tool_call_id"),
                "output": text_of(content)?,
            }));
            Ok(())
        }
        other => Err(format!(
            "message role {other:?} is not supported on a Responses-only model"
        )),
    }
}

fn push_user(content: &Value, input: &mut Vec<Value>) -> Result<(), String> {
    let parts = match content {
        Value::String(text) => vec![json!({ "type": "text", "text": text })],
        Value::Array(parts) => parts.clone(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for part in &parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = string_field(part, "text");
                if !text.is_empty() {
                    out.push(json!({ "type": "input_text", "text": text }));
                }
            }
            Some("image_url") => {
                let url = part
                    .pointer("/image_url/url")
                    .or_else(|| part.get("image_url"))
                    .and_then(Value::as_str)
                    .ok_or("image_url part without a url")?;
                out.push(json!({ "type": "input_image", "image_url": url }));
            }
            other => {
                return Err(format!(
                    "content part {other:?} is not supported on a Responses-only model"
                ));
            }
        }
    }
    if !out.is_empty() {
        input.push(json!({ "type": "message", "role": "user", "content": out }));
    }
    Ok(())
}

/// The assistant text becomes an `output_text` message, then each tool call a
/// `function_call` item, in the order the chat body gave them.
fn push_assistant(message: &Value, content: &Value, input: &mut Vec<Value>) -> Result<(), String> {
    let text = text_of(content)?;
    push_text_message("assistant", "output_text", &text, input);
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if call.get("type").and_then(Value::as_str) != Some("function") {
            return Err("only function tool calls are supported on a Responses-only model".into());
        }
        let function = call.get("function").unwrap_or(&Value::Null);
        input.push(json!({
            "type": "function_call",
            "call_id": string_field(call, "id"),
            "name": short_name(string_field(function, "name")),
            "arguments": function.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
        }));
    }
    Ok(())
}

fn push_text_message(role: &str, part_type: &str, text: &str, input: &mut Vec<Value>) {
    if text.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": role,
        "content": [{ "type": part_type, "text": text }],
    }));
}

/// The text of a message `content`: a string, or the text parts joined by a blank line.
fn text_of(content: &Value) -> Result<String, String> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => texts.push(string_field(part, "text")),
                    other => {
                        return Err(format!(
                            "content part {other:?} is not supported on a Responses-only model"
                        ));
                    }
                }
            }
            Ok(texts.join("\n\n"))
        }
        _ => Err("message content must be a string or an array of parts".into()),
    }
}

/// Chat `function` tools become top-level Responses function tools. Anything else is refused.
fn function_tools(body: &Value) -> Result<Vec<Value>, String> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    tools
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err("only function tools are supported on a Responses-only model".into());
            }
            let function = tool.get("function").unwrap_or(&Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or("function tool without a name")?;
            let mut out = Map::new();
            out.insert("type".into(), json!("function"));
            out.insert("name".into(), json!(name));
            if let Some(description) = function.get("description") {
                out.insert("description".into(), description.clone());
            }
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({}));
            out.insert("parameters".into(), normalize_tool_parameters(&parameters));
            out.insert("strict".into(), Value::Bool(false));
            Ok(Value::Object(out))
        })
        .collect()
}

fn tool_choice(choice: &Value) -> Result<Value, String> {
    match choice {
        Value::String(mode) if matches!(mode.as_str(), "auto" | "none" | "required") => {
            Ok(choice.clone())
        }
        Value::Object(_) if choice.get("type").and_then(Value::as_str) == Some("function") => {
            let name = choice
                .pointer("/function/name")
                .and_then(Value::as_str)
                .ok_or("tool_choice without a function name")?;
            Ok(json!({ "type": "function", "name": name }))
        }
        _ => Err("this tool_choice is not supported on a Responses-only model".into()),
    }
}

fn reasoning_effort(effort: &Value) -> Result<&str, String> {
    match effort.as_str() {
        Some(level @ ("minimal" | "low" | "medium" | "high")) => Ok(level),
        _ => Err(format!(
            "reasoning_effort {effort} is not supported on a Responses-only model"
        )),
    }
}

/// Puts a whole Responses reply into chat-completions shape. An unparseable body
/// becomes a chat-shaped error, so the client sees a readable failure.
#[must_use]
pub fn to_chat(body: &[u8], served_model: &str, names: &NameMap) -> Vec<u8> {
    let reply = match serde_json::from_slice::<Value>(body) {
        Ok(reply) => reply,
        Err(e) => {
            return chat_error_body(&format!("upstream returned a non-JSON responses body: {e}"));
        }
    };
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in reply
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(string_field(part, "text"));
                    }
                }
            }
            Some("function_call") => tool_calls.push(json!({
                "id": string_field(item, "call_id"),
                "type": "function",
                "function": {
                    "name": names.restore_name(string_field(item, "name")),
                    "arguments": string_field_or(item, "arguments", "{}"),
                },
            })),
            _ => {}
        }
    }
    let finish = finish_reason(&reply, !tool_calls.is_empty());
    let mut message = json!({ "role": "assistant", "content": text });
    if !tool_calls.is_empty() {
        if text.is_empty() {
            message["content"] = Value::Null;
        }
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let out = json!({
        "id": string_field(&reply, "id"),
        "object": "chat.completion",
        "created": reply.get("created_at").and_then(Value::as_u64).unwrap_or_default(),
        "model": served_model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": usage(&reply),
    });
    serde_json::to_vec(&out).unwrap_or_default()
}

/// A chat-completions error body, for a reply the hub cannot translate.
#[must_use]
pub fn chat_error_body(message: &str) -> Vec<u8> {
    let body = json!({ "error": { "message": message, "type": "upstream_error" } });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// `length` for a reply cut at `max_output_tokens`, `tool_calls` when it calls a tool, else `stop`.
fn finish_reason(response: &Value, saw_tool_call: bool) -> &'static str {
    match stop_reason(response, saw_tool_call) {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
    }
}

/// Chat usage from a Responses `usage` object, with cached input as `prompt_tokens_details`.
fn usage(response: &Value) -> Value {
    let field = |pointer: &str| {
        response
            .pointer(pointer)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let prompt = field("/usage/input_tokens");
    let completion = field("/usage/output_tokens");
    let mut usage = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    });
    let cached = field("/usage/input_tokens_details/cached_tokens");
    if cached > 0 {
        usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    usage
}

/// Translates one Responses SSE body into chat-completion chunks, as the bytes arrive.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stream-state flags"
)]
pub struct ChatStream {
    frames: SseFrames,
    served_model: String,
    names: NameMap,
    include_usage: bool,
    id: String,
    created: u64,
    role_sent: bool,
    tools: HashMap<u64, ToolSlot>,
    saw_tool_call: bool,
    done: bool,
}

/// One tool call, by its position in the chat `tool_calls` array.
#[derive(Debug, Clone, Copy)]
struct ToolSlot {
    index: u32,
    /// Whether any argument fragment went out, so the arguments on `output_item.done` are not sent twice.
    streamed: bool,
}

impl ChatStream {
    /// `served_model` is the qualified id the client sees. `include_usage` follows
    /// the request's `stream_options.include_usage`.
    #[must_use]
    pub fn new(served_model: String, names: NameMap, include_usage: bool) -> Self {
        Self {
            frames: SseFrames::new(),
            served_model,
            names,
            include_usage,
            id: String::new(),
            created: 0,
            role_sent: false,
            tools: HashMap::new(),
            saw_tool_call: false,
            done: false,
        }
    }

    /// Feeds raw upstream bytes and returns the chat chunks they produced.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = String::new();
        for frame in self.frames.push(bytes) {
            let Some(payload) = parse_data_frame(&frame) else {
                continue;
            };
            if let Ok(event) = serde_json::from_str::<Value>(payload.trim()) {
                self.handle_event(&event, &mut out);
            }
        }
        out.into_bytes()
    }

    /// End of upstream. Closes a stream the upstream left open; idempotent.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        if !self.done {
            self.terminal(&Value::Null, &mut out);
        }
        out.into_bytes()
    }

    fn handle_event(&mut self, event: &Value, out: &mut String) {
        if self.done {
            return;
        }
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.created" => {
                let response = event.get("response").unwrap_or(&Value::Null);
                self.id = string_field(response, "id").to_string();
                self.created = response
                    .get("created_at")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
            }
            "response.output_text.delta" => {
                let text = string_field(event, "delta");
                if !text.is_empty() {
                    self.emit(json!({ "content": text }), None, out);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item").filter(|item| is_function_call(item)) {
                    self.open_tool(output_index, item, out);
                }
            }
            "response.function_call_arguments.delta" => {
                let fragment = string_field(event, "delta");
                if let Some(slot) = self
                    .tools
                    .get_mut(&output_index)
                    .filter(|_| !fragment.is_empty())
                {
                    slot.streamed = true;
                    let index = slot.index;
                    self.emit_arguments(index, fragment, out);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item").filter(|item| is_function_call(item)) {
                    if !self.tools.contains_key(&output_index) {
                        self.open_tool(output_index, item, out);
                    }
                    let slot = self.tools[&output_index];
                    if !slot.streamed {
                        self.emit_arguments(
                            slot.index,
                            string_field_or(item, "arguments", ""),
                            out,
                        );
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.terminal(event.get("response").unwrap_or(&Value::Null), out);
            }
            "response.failed" | "error" => self.error(event, out),
            _ => {}
        }
    }

    fn open_tool(&mut self, output_index: u64, item: &Value, out: &mut String) {
        let index = u32::try_from(self.tools.len()).unwrap_or(u32::MAX);
        self.tools.insert(
            output_index,
            ToolSlot {
                index,
                streamed: false,
            },
        );
        self.saw_tool_call = true;
        let name = self
            .names
            .restore_name(string_field(item, "name"))
            .to_string();
        self.emit(
            json!({ "tool_calls": [{
                "index": index,
                "id": string_field(item, "call_id"),
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }] }),
            None,
            out,
        );
    }

    fn emit_arguments(&mut self, index: u32, fragment: &str, out: &mut String) {
        if fragment.is_empty() {
            return;
        }
        self.emit(
            json!({ "tool_calls": [{ "index": index, "function": { "arguments": fragment } }] }),
            None,
            out,
        );
    }

    /// The final chunk with its finish reason, the usage chunk when asked for, then `[DONE]`.
    fn terminal(&mut self, response: &Value, out: &mut String) {
        let finish = finish_reason(response, self.saw_tool_call);
        self.emit(json!({}), Some(finish), out);
        if self.include_usage {
            let chunk = json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.served_model,
                "choices": [],
                "usage": usage(response),
            });
            push_chunk(&chunk, out);
        }
        out.push_str("data: [DONE]\n\n");
        self.done = true;
    }

    fn error(&mut self, event: &Value, out: &mut String) {
        let message = event
            .pointer("/response/error/message")
            .or_else(|| event.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("the upstream responses stream failed");
        push_chunk(
            &json!({ "error": { "message": message, "type": "upstream_error" } }),
            out,
        );
        out.push_str("data: [DONE]\n\n");
        self.done = true;
    }

    /// One chunk with `delta`. The first one carries the assistant role.
    fn emit(&mut self, mut delta: Value, finish: Option<&str>, out: &mut String) {
        if !self.role_sent {
            self.role_sent = true;
            if let Some(fields) = delta.as_object_mut() {
                fields.insert("role".into(), json!("assistant"));
            }
        }
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.served_model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        push_chunk(&chunk, out);
    }
}

fn push_chunk(chunk: &Value, out: &mut String) {
    out.push_str("data: ");
    out.push_str(&serde_json::to_string(chunk).unwrap_or_else(|_| "{}".to_string()));
    out.push_str("\n\n");
}

fn is_function_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
}

fn string_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn string_field_or<'v>(value: &'v Value, key: &str, default: &'v str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    fn chunks(out: &[u8]) -> Vec<Value> {
        String::from_utf8(out.to_vec())
            .unwrap()
            .split("\n\n")
            .filter_map(|frame| frame.strip_prefix("data: "))
            .filter(|payload| *payload != "[DONE]")
            .map(|payload| serde_json::from_str(payload).unwrap())
            .collect()
    }

    #[test]
    fn chat_request_translates_to_responses() {
        let (out, _names) = chat_to_responses(&json!({
            "model": "copilot/gpt-5.5",
            "stream": false,
            "max_completion_tokens": 8,
            "reasoning_effort": "low",
            "parallel_tool_calls": false,
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}},
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "weather", "arguments": "{\"city\":\"Oslo\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"},
            ],
            "tools": [{"type": "function", "function": {
                "name": "weather", "description": "forecast",
                "parameters": {"properties": {"city": {"type": "string"}}},
            }}],
            "tool_choice": {"type": "function", "function": {"name": "weather"}},
        }))
        .unwrap();

        assert_eq!(
            out,
            json!({
                "model": "copilot/gpt-5.5",
                "stream": false,
                "input": [
                    {"type": "message", "role": "developer",
                     "content": [{"type": "input_text", "text": "be terse"}]},
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "look"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AA"},
                    ]},
                    {"type": "function_call", "call_id": "call_1", "name": "weather",
                     "arguments": "{\"city\":\"Oslo\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
                ],
                "tools": [{"type": "function", "name": "weather", "description": "forecast",
                           "parameters": {"properties": {"city": {"type": "string"}},
                                          "type": "object"},
                           "strict": false}],
                "tool_choice": {"type": "function", "name": "weather"},
                "parallel_tool_calls": false,
                "max_output_tokens": 16,
                "reasoning": {"effort": "low"},
                "store": false,
            })
        );
    }

    #[test]
    fn features_that_change_meaning_are_refused_not_dropped() {
        let base = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        let with = |key: &str, value: Value| {
            let mut body = base.clone();
            body[key] = value;
            body
        };
        let refused = [
            with("n", json!(2)),
            with("logprobs", json!(true)),
            with("response_format", json!({"type": "json_object"})),
            with("tools", json!([{"type": "web_search"}])),
            with("tool_choice", json!({"type": "custom", "name": "x"})),
            with("reasoning_effort", json!("turbo")),
            with(
                "messages",
                json!([{"role": "user", "content": [{"type": "input_audio"}]}]),
            ),
            with("messages", json!([{"role": "function", "content": "x"}])),
        ];
        for body in &refused {
            assert!(chat_to_responses(body).is_err(), "{body} should be refused");
        }
        assert!(chat_to_responses(&with("n", json!(1))).is_ok());
        assert!(chat_to_responses(&with("temperature", json!(0.2))).is_ok());
    }

    #[test]
    fn whole_reply_becomes_a_chat_completion_with_tool_calls_and_usage() {
        let reply = json!({
            "id": "resp_1",
            "created_at": 1700,
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": "checking"}]},
                {"type": "function_call", "call_id": "call_1", "name": "weather",
                 "arguments": "{\"city\":\"Oslo\"}"},
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4,
                      "input_tokens_details": {"cached_tokens": 6}},
        });
        let out: Value = serde_json::from_slice(&to_chat(
            reply.to_string().as_bytes(),
            "copilot/gpt-5.5",
            &NameMap::default(),
        ))
        .unwrap();

        assert_eq!(
            out,
            json!({
                "id": "resp_1",
                "object": "chat.completion",
                "created": 1700,
                "model": "copilot/gpt-5.5",
                "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{"id": "call_1", "type": "function",
                                    "function": {"name": "weather",
                                                 "arguments": "{\"city\":\"Oslo\"}"}}],
                }}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
                          "prompt_tokens_details": {"cached_tokens": 6}},
            })
        );
    }

    #[test]
    fn a_reply_cut_at_max_output_tokens_finishes_with_length() {
        let out: Value = serde_json::from_slice(&to_chat(
            json!({
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "par"}]}],
            })
            .to_string()
            .as_bytes(),
            "m",
            &NameMap::default(),
        ))
        .unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "length");
        assert_eq!(out["choices"][0]["message"]["content"], "par");
    }

    #[test]
    fn stream_becomes_chat_chunks_and_ends_with_done() {
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_s", "created_at": 100}}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"}),
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "call_1", "name": "weather"}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"city\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "\"Oslo\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "call_1", "name": "weather",
                            "arguments": "{\"city\":\"Oslo\"}"}}),
            json!({"type": "response.completed", "response": {"id": "resp_s",
                   "status": "completed", "usage": {"input_tokens": 9, "output_tokens": 4}}}),
        ];
        let mut stream = ChatStream::new("copilot/gpt-5.5".into(), NameMap::default(), true);
        let wire = events.iter().fold(String::new(), |mut wire, event| {
            let _ = write!(wire, "event: x\ndata: {event}\n\n");
            wire
        });
        let mut out = stream.push(wire.as_bytes());
        out.extend(stream.finish());

        let base = |delta: Value, finish: Value| {
            json!({"id": "resp_s", "object": "chat.completion.chunk", "created": 100,
                   "model": "copilot/gpt-5.5",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        };
        assert_eq!(
            chunks(&out),
            vec![
                base(json!({"role": "assistant", "content": "hi"}), Value::Null),
                base(
                    json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                            "function": {"name": "weather", "arguments": ""}}]}),
                    Value::Null
                ),
                base(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}),
                    Value::Null
                ),
                base(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Oslo\"}"}}]}),
                    Value::Null
                ),
                base(json!({}), json!("tool_calls")),
                json!({"id": "resp_s", "object": "chat.completion.chunk", "created": 100,
                       "model": "copilot/gpt-5.5", "choices": [],
                       "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}}),
            ]
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .ends_with("data: [DONE]\n\n")
        );
    }

    #[test]
    fn a_stream_cut_short_still_closes_with_done() {
        let mut stream = ChatStream::new("m".into(), NameMap::default(), false);
        let mut out =
            stream.push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n");
        out.extend(stream.finish());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(!text.contains("\"usage\""));
    }
}

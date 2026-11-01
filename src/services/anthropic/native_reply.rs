//! Anthropic Messages reply -> `OpenAI` chat completion, for Copilot Claude models that
//! answer `/chat/completions` in their native shape. An `OpenAI` reply passes through.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::consts::MAX_TRANSFORM_BUFFER_BYTES;
use crate::services::sse::{SseFrames, parse_data_frame};
use crate::services::store::now_ms;

const DONE_FRAME: &str = "data: [DONE]\n\n";
const CHAT_COMPLETION: &str = "chat.completion";
const CHAT_COMPLETION_CHUNK: &str = "chat.completion.chunk";
/// The `type` of every Anthropic stream event; an `OpenAI` chunk has none.
const ANTHROPIC_EVENTS: [&str; 8] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    "error",
];

/// Rewrites one upstream reply into `OpenAI` shape when it turns out to be an
/// Anthropic one. Fed chunk by chunk, like a response transform.
#[derive(Debug)]
pub enum NativeReplyRepair {
    /// A non-SSE body, held until the end, or forwarded untouched past the buffer cap.
    Body(BufferedReply),
    /// An SSE body, rewritten one frame at a time.
    Stream(Box<NativeReplyStream>),
}

impl NativeReplyRepair {
    /// A repair for an SSE body when `is_sse`, else for a buffered one.
    #[must_use]
    pub fn new(is_sse: bool) -> Self {
        if is_sse {
            Self::Stream(Box::default())
        } else {
            Self::Body(BufferedReply::default())
        }
    }

    /// Feeds one upstream chunk and returns the bytes to send on. May be empty.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Body(body) => body.push(bytes),
            Self::Stream(stream) => stream.push(bytes),
        }
    }

    /// End of upstream: whatever is still owed to the client. Idempotent.
    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Body(body) => body.finish(),
            Self::Stream(stream) => stream.finish(),
        }
    }
}

/// A whole non-SSE reply, converted at the end when it is Anthropic-shaped.
#[derive(Debug, Default)]
pub struct BufferedReply {
    buf: Vec<u8>,
    /// Set once the body outgrew the cap; everything after streams through.
    overflowed: bool,
}

impl BufferedReply {
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.overflowed {
            return bytes.to_vec();
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > MAX_TRANSFORM_BUFFER_BYTES {
            self.overflowed = true;
            return std::mem::take(&mut self.buf);
        }
        Vec::new()
    }

    fn finish(&mut self) -> Vec<u8> {
        let buf = std::mem::take(&mut self.buf);
        to_chat_completion(&buf).unwrap_or(buf)
    }
}

/// Converts an Anthropic message body into an `OpenAI` chat completion. Returns `None` for
/// anything else, including a body that already carries `choices`.
#[must_use]
pub fn to_chat_completion(body: &[u8]) -> Option<Vec<u8>> {
    let message: Value = serde_json::from_slice(body).ok()?;
    if !is_anthropic_message(&message) {
        return None;
    }
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in blocks {
        match str_field(block, "type") {
            "text" => text.push_str(str_field(block, "text")),
            "thinking" => reasoning.push_str(str_field(block, "thinking")),
            "tool_use" => tool_calls.push(json!({
                "id": str_field(block, "id"),
                "type": "function",
                "function": {
                    "name": str_field(block, "name"),
                    "arguments": tool_arguments(block.get("input")),
                },
            })),
            _ => {}
        }
    }

    let finish_reason = finish_reason(
        message.get("stop_reason").and_then(Value::as_str),
        !tool_calls.is_empty(),
        !text.is_empty(),
    );
    let mut reply = Map::new();
    reply.insert("role".to_string(), json!("assistant"));
    // `OpenAI` sends `null` content next to tool calls, never an empty string.
    let content = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    reply.insert("content".to_string(), content);
    if !reasoning.is_empty() {
        reply.insert("reasoning_content".to_string(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        reply.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let mut completion = json!({
        "id": str_field(&message, "id"),
        "object": CHAT_COMPLETION,
        "created": now_ms() / 1000,
        "model": str_field(&message, "model"),
        "choices": [{"index": 0, "message": reply, "finish_reason": finish_reason}],
    });
    if let Some(usage) = message.get("usage").and_then(Value::as_object) {
        completion["usage"] = openai_usage(usage);
    }
    serde_json::to_vec(&completion).ok()
}

/// An Anthropic message, as opposed to a chat completion: no `choices`, and
/// either the `message` type or a content-block array.
fn is_anthropic_message(value: &Value) -> bool {
    let has_choices = value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| !choices.is_empty());
    !has_choices
        && (str_field(value, "type") == "message"
            || value.get("content").is_some_and(Value::is_array))
}

/// Anthropic `stop_reason` -> `OpenAI` `finish_reason`. Tool calls always report `tool_calls`
/// (they drive the client's tool loop); an empty reply with no reason reports `length`.
fn finish_reason(stop_reason: Option<&str>, saw_tool_call: bool, saw_text: bool) -> &'static str {
    if saw_tool_call {
        return "tool_calls";
    }
    match stop_reason {
        Some("end_turn" | "stop_sequence" | "pause_turn") => "stop",
        Some("max_tokens" | "model_context_window_exceeded") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ if saw_text => "stop",
        _ => "length",
    }
}

/// Anthropic usage -> `OpenAI` usage, with cache reads and writes added to `prompt_tokens`.
/// Cache writes keep their Anthropic name too, which is where the stats scraper reads them.
fn openai_usage(usage: &Map<String, Value>) -> Value {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = field("cache_read_input_tokens");
    let cache_write = field("cache_creation_input_tokens");
    let prompt = field("input_tokens") + cache_read + cache_write;
    let completion = field("output_tokens");
    let mut mapped = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
        "prompt_tokens_details": {"cached_tokens": cache_read},
    });
    if cache_write > 0 {
        mapped["cache_creation_input_tokens"] = json!(cache_write);
    }
    mapped
}

/// `tool_use.input` as the JSON string `OpenAI` puts in `arguments`.
fn tool_arguments(input: Option<&Value>) -> String {
    match input {
        Some(value @ Value::Object(_)) => value.to_string(),
        _ => "{}".to_string(),
    }
}

fn str_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Anthropic Messages SSE -> `OpenAI` chat-completion chunks.
#[derive(Default)]
pub struct NativeReplyStream {
    frames: SseFrames,
    mode: Mode,
    /// Upstream bytes held while `Detecting`, forwarded as-is on passthrough.
    raw: Vec<u8>,
    id: String,
    model: String,
    created: u64,
    /// Anthropic content-block index -> `OpenAI` `tool_calls[].index`.
    tool_indices: HashMap<u64, u64>,
    saw_text: bool,
    finish_sent: bool,
    /// Anthropic usage fields, latest value wins (`message_start` carries the
    /// input counts, `message_delta` the output count).
    usage: Map<String, Value>,
    done: bool,
}

impl std::fmt::Debug for NativeReplyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeReplyStream")
            .field("mode", &self.mode)
            .field("model", &self.model)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No `data:` frame yet, so the shape is still unknown.
    #[default]
    Detecting,
    /// The upstream already speaks `OpenAI`; bytes flow through untouched.
    Passthrough,
    Converting,
}

impl NativeReplyStream {
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.mode {
            Mode::Passthrough => bytes.to_vec(),
            Mode::Converting => {
                let mut out = String::new();
                for frame in self.frames.push(bytes) {
                    self.handle_frame(&frame, &mut out);
                }
                out.into_bytes()
            }
            Mode::Detecting => self.detect(bytes),
        }
    }

    fn detect(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.raw.extend_from_slice(bytes);
        let mut frames = self.frames.push(bytes).into_iter();
        let mut out = String::new();
        for frame in frames.by_ref() {
            let Some(data) = parse_data_frame(&frame) else {
                continue;
            };
            if !is_anthropic_event(&data) {
                return self.pass_through();
            }
            self.mode = Mode::Converting;
            self.raw = Vec::new();
            self.handle_frame(&frame, &mut out);
            break;
        }
        for frame in frames {
            self.handle_frame(&frame, &mut out);
        }
        if self.mode == Mode::Detecting && self.raw.len() > MAX_TRANSFORM_BUFFER_BYTES {
            return self.pass_through();
        }
        out.into_bytes()
    }

    fn pass_through(&mut self) -> Vec<u8> {
        self.mode = Mode::Passthrough;
        self.frames.take_pending();
        std::mem::take(&mut self.raw)
    }

    fn finish(&mut self) -> Vec<u8> {
        match self.mode {
            Mode::Detecting => self.pass_through(),
            Mode::Passthrough => Vec::new(),
            Mode::Converting if self.done => Vec::new(),
            Mode::Converting => {
                // Cut off before `message_stop`: still leave the client with
                // the usage seen so far and the `[DONE]` it waits for.
                let mut out = String::new();
                self.finish_stream(&mut out);
                out.into_bytes()
            }
        }
    }

    fn handle_frame(&mut self, frame: &str, out: &mut String) {
        if self.done {
            return;
        }
        let Some(event) = parse_data_frame(frame).and_then(|data| serde_json::from_str(&data).ok())
        else {
            return;
        };
        match str_field(&event, "type") {
            "message_start" => self.on_message_start(&event, out),
            "content_block_start" => self.on_block_start(&event, out),
            "content_block_delta" => self.on_block_delta(&event, out),
            "message_delta" => self.on_message_delta(&event, out),
            "message_stop" => self.finish_stream(out),
            "error" => {
                let error = event.get("error").cloned().unwrap_or_else(|| json!({}));
                push_data(out, &json!({ "error": error }));
                self.done = true;
            }
            _ => {}
        }
    }

    fn on_message_start(&mut self, event: &Value, out: &mut String) {
        let message = event.get("message").unwrap_or(&Value::Null);
        str_field(message, "id").clone_into(&mut self.id);
        str_field(message, "model").clone_into(&mut self.model);
        self.created = now_ms() / 1000;
        self.absorb_usage(message.get("usage"));
        self.push_chunk(out, &json!({"role": "assistant", "content": ""}), None);
    }

    fn on_block_start(&mut self, event: &Value, out: &mut String) {
        let block = event.get("content_block").unwrap_or(&Value::Null);
        match str_field(block, "type") {
            "text" => self.push_text(str_field(block, "text"), out),
            "thinking" => self.push_reasoning(str_field(block, "thinking"), out),
            "tool_use" => {
                let block_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let tool_index = self.tool_indices.len() as u64;
                self.tool_indices.insert(block_index, tool_index);
                // The arguments normally follow as `input_json_delta`s; a
                // non-empty `input` here is the whole of them.
                let arguments = match block.get("input") {
                    Some(Value::Object(input)) if !input.is_empty() => {
                        tool_arguments(block.get("input"))
                    }
                    _ => String::new(),
                };
                let call = json!({
                    "index": tool_index,
                    "id": str_field(block, "id"),
                    "type": "function",
                    "function": {"name": str_field(block, "name"), "arguments": arguments},
                });
                self.push_chunk(out, &json!({ "tool_calls": [call] }), None);
            }
            _ => {}
        }
    }

    fn on_block_delta(&mut self, event: &Value, out: &mut String) {
        let delta = event.get("delta").unwrap_or(&Value::Null);
        match str_field(delta, "type") {
            "text_delta" => self.push_text(str_field(delta, "text"), out),
            "thinking_delta" => self.push_reasoning(str_field(delta, "thinking"), out),
            "input_json_delta" => {
                let block_index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let Some(&tool_index) = self.tool_indices.get(&block_index) else {
                    return;
                };
                let call = json!({
                    "index": tool_index,
                    "function": {"arguments": str_field(delta, "partial_json")},
                });
                self.push_chunk(out, &json!({ "tool_calls": [call] }), None);
            }
            _ => {}
        }
    }

    fn on_message_delta(&mut self, event: &Value, out: &mut String) {
        self.absorb_usage(event.get("usage"));
        let stop_reason = event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str);
        let reason = finish_reason(stop_reason, !self.tool_indices.is_empty(), self.saw_text);
        self.push_chunk(out, &json!({}), Some(reason));
        self.finish_sent = true;
    }

    /// The trailing usage chunk (`choices: []`, as `stream_options.include_usage`
    /// produces) and `[DONE]`, exactly once.
    fn finish_stream(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        self.done = true;
        if !self.usage.is_empty() {
            push_data(
                out,
                &json!({
                    "id": self.id,
                    "object": CHAT_COMPLETION_CHUNK,
                    "created": self.created,
                    "model": self.model,
                    "choices": [],
                    "usage": openai_usage(&self.usage),
                }),
            );
        }
        out.push_str(DONE_FRAME);
    }

    fn push_text(&mut self, text: &str, out: &mut String) {
        if text.is_empty() {
            return;
        }
        self.saw_text = true;
        self.push_chunk(out, &json!({ "content": text }), None);
    }

    fn push_reasoning(&self, text: &str, out: &mut String) {
        if !text.is_empty() {
            self.push_chunk(out, &json!({ "reasoning_content": text }), None);
        }
    }

    fn push_chunk(&self, out: &mut String, delta: &Value, finish_reason: Option<&str>) {
        if self.finish_sent {
            return;
        }
        push_data(
            out,
            &json!({
                "id": self.id,
                "object": CHAT_COMPLETION_CHUNK,
                "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
            }),
        );
    }

    fn absorb_usage(&mut self, usage: Option<&Value>) {
        if let Some(usage) = usage.and_then(Value::as_object) {
            for (key, value) in usage {
                self.usage.insert(key.clone(), value.clone());
            }
        }
    }
}

fn is_anthropic_event(data: &str) -> bool {
    serde_json::from_str::<Value>(data)
        .ok()
        .is_some_and(|event| ANTHROPIC_EVENTS.contains(&str_field(&event, "type")))
}

fn push_data(out: &mut String, payload: &Value) {
    out.push_str("data: ");
    out.push_str(&payload.to_string());
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(body: &Value) -> Value {
        let bytes = to_chat_completion(body.to_string().as_bytes()).expect("converted");
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn text_reply_becomes_a_chat_completion_with_mapped_usage() {
        let reply = completion(&json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4.8",
            "content": [{"type": "text", "text": "hel"}, {"type": "text", "text": "lo"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2, "cache_read_input_tokens": 10},
        }));

        assert_eq!(reply["object"], "chat.completion");
        assert_eq!(reply["id"], "msg_1");
        assert_eq!(reply["model"], "claude-opus-4.8");
        let choice = &reply["choices"][0];
        assert_eq!(choice["message"]["role"], "assistant");
        assert_eq!(choice["message"]["content"], "hello");
        assert_eq!(choice["finish_reason"], "stop");
        assert_eq!(reply["usage"]["prompt_tokens"], 13);
        assert_eq!(reply["usage"]["completion_tokens"], 2);
        assert_eq!(reply["usage"]["total_tokens"], 15);
        assert_eq!(reply["usage"]["prompt_tokens_details"]["cached_tokens"], 10);
    }

    #[test]
    fn tool_use_and_thinking_blocks_map_to_tool_calls_and_reasoning() {
        let reply = completion(&json!({
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "plan", "signature": "s"},
                {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a"}},
            ],
            "stop_reason": "end_turn",
        }));

        let message = &reply["choices"][0]["message"];
        assert_eq!(message["content"], Value::Null);
        assert_eq!(message["reasoning_content"], "plan");
        assert_eq!(message["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a"}"#
        );
        assert_eq!(reply["choices"][0]["finish_reason"], "tool_calls");
        assert!(reply.get("usage").is_none());
    }

    #[test]
    fn stop_reasons_map_to_finish_reasons() {
        let reason = |stop: Value, content: Value| {
            completion(&json!({"type": "message", "content": content, "stop_reason": stop}))
                ["choices"][0]["finish_reason"]
                .clone()
        };
        let text = json!([{"type": "text", "text": "x"}]);
        assert_eq!(reason(json!("max_tokens"), text.clone()), "length");
        assert_eq!(reason(json!("stop_sequence"), text.clone()), "stop");
        assert_eq!(reason(json!("refusal"), text.clone()), "content_filter");
        assert_eq!(reason(Value::Null, text), "stop");
        // The max_tokens=1 probe: no content and no reason.
        assert_eq!(reason(Value::Null, json!([])), "length");
    }

    #[test]
    fn openai_bodies_and_non_json_are_left_alone() {
        let openai = json!({"choices": [{"message": {"content": "x"}}], "content": []});
        assert!(to_chat_completion(openai.to_string().as_bytes()).is_none());
        assert!(to_chat_completion(br#"{"error":{"message":"x"}}"#).is_none());
        assert!(to_chat_completion(b"not json").is_none());
    }

    #[test]
    fn buffered_reply_converts_at_the_end_only() {
        let mut repair = NativeReplyRepair::new(false);
        let body = json!({"type": "message", "content": [{"type": "text", "text": "hi"}]});
        let bytes = body.to_string().into_bytes();
        let (head, tail) = bytes.split_at(5);
        assert!(repair.push(head).is_empty());
        assert!(repair.push(tail).is_empty());
        let reply: Value = serde_json::from_slice(&repair.finish()).unwrap();
        assert_eq!(reply["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn buffered_openai_reply_comes_back_byte_for_byte() {
        let mut repair = NativeReplyRepair::new(false);
        let body = br#"{"id":"c","choices":[{"index":0,"message":{"content":"x"}}]}"#;
        assert!(repair.push(body).is_empty());
        assert_eq!(repair.finish(), body.to_vec());
    }

    fn event(name: &str, data: &Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn anthropic_stream() -> String {
        [
            event(
                "message_start",
                &json!({"type": "message_start", "message": {
                    "id": "msg_1", "model": "claude-opus-4.8",
                    "usage": {"input_tokens": 7, "output_tokens": 1},
                }}),
            ),
            event("ping", &json!({"type": "ping"})),
            event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
            ),
            event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "hi"}}),
            ),
            event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 0}),
            ),
            event(
                "content_block_start",
                &json!({"type": "content_block_start", "index": 1,
                    "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}}}),
            ),
            event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
            ),
            event(
                "content_block_delta",
                &json!({"type": "content_block_delta", "index": 1,
                    "delta": {"type": "input_json_delta", "partial_json": "\"a\"}"}}),
            ),
            event(
                "message_delta",
                &json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 9}}),
            ),
            event("message_stop", &json!({"type": "message_stop"})),
        ]
        .concat()
    }

    /// The `data:` payloads of an `OpenAI` stream, in order.
    fn chunks(bytes: &[u8]) -> Vec<String> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter_map(|frame| frame.strip_prefix("data: "))
            .map(str::to_string)
            .collect()
    }

    fn run_stream(input: &str, split: usize) -> Vec<u8> {
        let mut repair = NativeReplyRepair::new(true);
        let mut out = Vec::new();
        for piece in input.as_bytes().chunks(split) {
            out.extend(repair.push(piece));
        }
        out.extend(repair.finish());
        out
    }

    #[test]
    fn anthropic_stream_becomes_openai_chunks() {
        // A small split proves frames are reassembled across chunk borders.
        let out = run_stream(&anthropic_stream(), 7);
        let frames = chunks(&out);
        assert_eq!(frames.last().unwrap(), "[DONE]");
        let parsed: Vec<Value> = frames[..frames.len() - 1]
            .iter()
            .map(|frame| serde_json::from_str(frame).unwrap())
            .collect();
        let delta = |i: usize| &parsed[i]["choices"][0]["delta"];

        assert!(
            parsed
                .iter()
                .all(|c| c["object"] == "chat.completion.chunk")
        );
        assert!(parsed.iter().all(|c| c["id"] == "msg_1"));
        assert_eq!(delta(0)["role"], "assistant");
        assert_eq!(delta(1)["content"], "hi");
        assert_eq!(delta(2)["tool_calls"][0]["index"], 0);
        assert_eq!(delta(2)["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(delta(2)["tool_calls"][0]["function"]["name"], "read");
        let arguments: String = [3, 4]
            .iter()
            .map(|&i| {
                delta(i)["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(arguments, r#"{"path":"a"}"#);
        assert_eq!(parsed[5]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(parsed[6]["choices"], json!([]));
        assert_eq!(parsed[6]["usage"]["prompt_tokens"], 7);
        assert_eq!(parsed[6]["usage"]["completion_tokens"], 9);
        assert_eq!(parsed.len(), 7);
    }

    #[test]
    fn openai_stream_passes_through_byte_for_byte() {
        let input = concat!(
            ": keepalive\n\n",
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(run_stream(input, 5), input.as_bytes());
    }

    #[test]
    fn cut_off_anthropic_stream_still_ends_with_done() {
        let input = event(
            "message_start",
            &json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 2}}}),
        );
        let frames = chunks(&run_stream(&input, 1024));
        assert_eq!(frames.len(), 3, "{frames:?}");
        let usage: Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(usage["usage"]["prompt_tokens"], 2);
        assert_eq!(frames[2], "[DONE]");
    }

    #[test]
    fn anthropic_error_event_becomes_an_openai_error_frame() {
        let input = event(
            "error",
            &json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
        );
        let frames = chunks(&run_stream(&input, 1024));
        assert_eq!(frames.len(), 1, "{frames:?}");
        let error: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(error["error"]["message"], "busy");
    }
}

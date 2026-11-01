//! `OpenAI` Responses SSE -> Anthropic Messages SSE, one block open at a time. It always
//! closes the message it opened, even when the upstream stops early.

use serde_json::{Value, json};

use crate::consts::ANTHROPIC_THINKING_SIGNATURE;
use crate::schemas::anthropic_error::error_type_for_status;
use crate::services::anthropic::response::qualify_message_id;
use crate::services::anthropic::responses_message::{stop_reason, usage};
use crate::services::sse::{SseFrames, format_event, parse_data_frame};
use crate::services::tool_names::NameMap;

const STREAM_ERROR_STATUS: u16 = 500;

/// Translates one Copilot `/responses` SSE body into Anthropic Messages SSE, chunk by chunk.
pub struct ResponsesStream {
    frames: SseFrames,
    served_model: String,
    names: NameMap,
    phase: Phase,
    next_index: u32,
    open: Option<OpenBlock>,
    saw_tool_use: bool,
}

impl std::fmt::Debug for ResponsesStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesStream")
            .field("served_model", &self.served_model)
            .field("phase", &self.phase)
            .field("next_index", &self.next_index)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    Open,
    Finished,
}

#[derive(Debug, Clone, Copy)]
enum OpenBlock {
    Thinking(u32),
    Text(u32),
    Tool {
        index: u32,
        output_index: u64,
        /// Whether any argument fragment went out, so the arguments on
        /// `output_item.done` are not sent twice.
        streamed: bool,
    },
}

impl OpenBlock {
    fn index(self) -> u32 {
        match self {
            Self::Thinking(index) | Self::Text(index) | Self::Tool { index, .. } => index,
        }
    }
}

impl ResponsesStream {
    /// `served_model` is the qualified `<profile>/<model>` the client sees in
    /// `message_start`. `names` undoes the request's tool name cuts.
    #[must_use]
    pub fn new(served_model: String, names: NameMap) -> Self {
        Self {
            frames: SseFrames::new(),
            served_model,
            names,
            phase: Phase::Pending,
            next_index: 0,
            open: None,
            saw_tool_use: false,
        }
    }

    /// Feeds raw upstream bytes and returns the Anthropic events they produced.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = String::new();
        for frame in self.frames.push(bytes) {
            let Some(payload) = parse_data_frame(&frame) else {
                continue;
            };
            match serde_json::from_str::<Value>(payload.trim()) {
                Ok(event) => self.handle_event(&event, &mut out),
                Err(e) => tracing::debug!("ignoring unparseable responses sse frame: {e}"),
            }
        }
        out.into_bytes()
    }

    /// End of upstream. Closes a message the upstream left open; idempotent.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        if self.phase == Phase::Open {
            self.emit_terminal(&Value::Null, &mut out);
        }
        out.into_bytes()
    }

    fn handle_event(&mut self, event: &Value, out: &mut String) {
        if self.phase == Phase::Finished {
            return;
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let failed = matches!(kind, "response.failed" | "error");
        if self.phase == Phase::Pending && !failed {
            self.emit_message_start(event, out);
        }
        let output_index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match kind {
            "response.output_item.added" => {
                if let Some(item) = event.get("item").filter(|item| is_function_call(item)) {
                    self.open_tool_block(item, output_index, out);
                }
            }
            "response.output_text.delta" => self.text_delta(event, out),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.thinking_delta(event, out);
            }
            "response.function_call_arguments.delta" => {
                let fragment = str_field(event, "delta");
                self.arguments_delta(output_index, fragment, out);
            }
            "response.output_item.done" => self.item_done(event, output_index, out),
            "response.completed" | "response.incomplete" => {
                self.emit_terminal(event.get("response").unwrap_or(&Value::Null), out);
            }
            _ if failed => self.emit_error(event, out),
            _ => {}
        }
    }

    fn emit_message_start(&mut self, event: &Value, out: &mut String) {
        let id = event
            .pointer("/response/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        out.push_str(&format_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": qualify_message_id(id),
                    "type": "message",
                    "role": "assistant",
                    "model": self.served_model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        ));
        self.phase = Phase::Open;
    }

    fn text_delta(&mut self, event: &Value, out: &mut String) {
        let text = str_field(event, "delta");
        if text.is_empty() {
            return;
        }
        let index = match self.open {
            Some(OpenBlock::Text(index)) => index,
            _ => self.open_block(OpenBlock::Text, &json!({"type": "text", "text": ""}), out),
        };
        push_delta(index, &json!({"type": "text_delta", "text": text}), out);
    }

    fn thinking_delta(&mut self, event: &Value, out: &mut String) {
        let thinking = str_field(event, "delta");
        if thinking.is_empty() {
            return;
        }
        let index = match self.open {
            Some(OpenBlock::Thinking(index)) => index,
            _ => self.open_block(
                OpenBlock::Thinking,
                &json!({"type": "thinking", "thinking": ""}),
                out,
            ),
        };
        push_delta(
            index,
            &json!({"type": "thinking_delta", "thinking": thinking}),
            out,
        );
    }

    fn arguments_delta(&mut self, output_index: u64, fragment: &str, out: &mut String) {
        let Some(OpenBlock::Tool {
            index,
            output_index: open_index,
            streamed,
        }) = self.open.as_mut()
        else {
            return;
        };
        if *open_index != output_index || fragment.is_empty() {
            return;
        }
        *streamed = true;
        push_delta(
            *index,
            &json!({"type": "input_json_delta", "partial_json": fragment}),
            out,
        );
    }

    /// A finished function call sends its arguments when none streamed, and
    /// opens its block first when `output_item.added` never arrived.
    fn item_done(&mut self, event: &Value, output_index: u64, out: &mut String) {
        let Some(item) = event.get("item") else {
            return;
        };
        if !is_function_call(item) {
            if !matches!(self.open, Some(OpenBlock::Tool { .. })) {
                self.close_block(out);
            }
            return;
        }
        let open_here = matches!(
            self.open,
            Some(OpenBlock::Tool { output_index: open_index, .. }) if open_index == output_index
        );
        if !open_here {
            self.open_tool_block(item, output_index, out);
        }
        if let Some(OpenBlock::Tool {
            streamed: false, ..
        }) = self.open
        {
            self.arguments_delta(output_index, str_field(item, "arguments"), out);
        }
        self.close_block(out);
    }

    fn open_tool_block(&mut self, item: &Value, output_index: u64, out: &mut String) {
        let name = self.names.restore_name(str_field(item, "name")).to_string();
        let block = json!({
            "type": "tool_use",
            "id": str_field(item, "call_id"),
            "name": name,
            "input": {},
        });
        self.saw_tool_use = true;
        self.open_block(
            |index| OpenBlock::Tool {
                index,
                output_index,
                streamed: false,
            },
            &block,
            out,
        );
    }

    fn open_block(
        &mut self,
        make: impl FnOnce(u32) -> OpenBlock,
        content_block: &Value,
        out: &mut String,
    ) -> u32 {
        self.close_block(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(make(index));
        out.push_str(&format_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            }),
        ));
        index
    }

    fn close_block(&mut self, out: &mut String) {
        let Some(block) = self.open.take() else {
            return;
        };
        // Claude Code drops a thinking block that ends without a signature.
        if let OpenBlock::Thinking(index) = block {
            push_delta(
                index,
                &json!({"type": "signature_delta", "signature": ANTHROPIC_THINKING_SIGNATURE}),
                out,
            );
        }
        out.push_str(&format_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": block.index()}),
        ));
    }

    fn emit_terminal(&mut self, response: &Value, out: &mut String) {
        self.close_block(out);
        out.push_str(&format_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason(response, self.saw_tool_use),
                    "stop_sequence": Value::Null,
                },
                "usage": usage(response),
            }),
        ));
        out.push_str(&format_event(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
        self.phase = Phase::Finished;
    }

    fn emit_error(&mut self, event: &Value, out: &mut String) {
        let message = event
            .pointer("/response/error/message")
            .or_else(|| event.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("the upstream responses stream failed");
        self.close_block(out);
        out.push_str(&format_event(
            "error",
            &json!({
                "type": "error",
                "error": {"type": error_type_for_status(STREAM_ERROR_STATUS), "message": message},
            }),
        ));
        self.phase = Phase::Finished;
    }
}

fn push_delta(index: u32, delta: &Value, out: &mut String) {
    out.push_str(&format_event(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": index, "delta": delta}),
    ));
}

fn is_function_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
}

fn str_field<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>()
            .concat()
    }

    fn run(events: &[Value], names: NameMap) -> Vec<(String, Value)> {
        let mut stream = ResponsesStream::new("copilot/gpt-5.5".to_string(), names);
        let mut bytes = stream.push(frames(events).as_bytes());
        bytes.extend(stream.finish());
        String::from_utf8(bytes)
            .unwrap()
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(|frame| {
                let mut lines = frame.lines();
                let name = lines
                    .next()
                    .unwrap()
                    .strip_prefix("event: ")
                    .unwrap()
                    .to_string();
                let data = lines.next().unwrap().strip_prefix("data: ").unwrap();
                (name, serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn completed(usage: &Value) -> Value {
        json!({"type": "response.completed", "response": {"status": "completed", "usage": usage}})
    }

    #[test]
    fn text_deltas_become_one_text_block_and_completion_carries_usage() {
        let events = run(
            &[
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "m"}}),
                json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "m", "delta": "he"}),
                json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "m", "delta": "llo"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "m"}}),
                completed(&json!({"input_tokens": 7, "output_tokens": 2})),
            ],
            NameMap::default(),
        );
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0].1["message"]["id"], "msg_resp_1");
        assert_eq!(events[0].1["message"]["model"], "copilot/gpt-5.5");
        assert_eq!(
            events[3].1["delta"],
            json!({"type": "text_delta", "text": "llo"})
        );
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            events[5].1["usage"],
            json!({"input_tokens": 7, "output_tokens": 2})
        );
    }

    #[test]
    fn function_call_streams_as_tool_use_with_input_json_deltas() {
        let events = run(
            &[
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_item.added", "output_index": 0,
                       "item": {"type": "function_call", "id": "fc", "call_id": "call_1", "name": "search"}}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"q\":"}),
                json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "\"rust\"}"}),
                json!({"type": "response.output_item.done", "output_index": 0,
                       "item": {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"rust\"}"}}),
                completed(&json!({"input_tokens": 1, "output_tokens": 1})),
            ],
            NameMap::default(),
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "tool_use", "id": "call_1", "name": "search", "input": {}})
        );
        let fragments: Vec<&str> = events
            .iter()
            .filter_map(|(_, data)| data["delta"]["partial_json"].as_str())
            .collect();
        assert_eq!(fragments, ["{\"q\":", "\"rust\"}"]);
        assert_eq!(events[4].0, "content_block_stop");
        assert_eq!(events[5].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn arguments_only_on_item_done_are_sent_once() {
        let events = run(
            &[
                json!({"type": "response.output_item.done", "output_index": 1,
                       "item": {"type": "function_call", "call_id": "call_2", "name": "read", "arguments": "{}"}}),
                completed(&json!({})),
            ],
            NameMap::default(),
        );
        assert_eq!(
            names(&events),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[2].1["delta"]["partial_json"], "{}");
    }

    #[test]
    fn reasoning_summary_becomes_a_signed_thinking_block_before_text() {
        let events = run(
            &[
                json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "plan"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning"}}),
                json!({"type": "response.output_text.delta", "output_index": 1, "delta": "ok"}),
                completed(&json!({})),
            ],
            NameMap::default(),
        );
        assert_eq!(events[1].1["content_block"]["type"], "thinking");
        assert_eq!(events[3].1["delta"]["type"], "signature_delta");
        assert_eq!(events[5].1["content_block"]["type"], "text");
        assert_eq!(events[5].1["index"], 1);
    }

    #[test]
    fn truncated_upstream_still_closes_the_message() {
        let events = run(
            &[json!({"type": "response.output_text.delta", "output_index": 0, "delta": "par"})],
            NameMap::default(),
        );
        assert_eq!(
            names(&events)[3..],
            ["content_block_stop", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn failed_response_becomes_an_anthropic_error_event() {
        let events = run(
            &[
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "x"}),
                json!({"type": "response.failed", "response": {"error": {"message": "boom"}}}),
            ],
            NameMap::default(),
        );
        let (name, data) = events.last().unwrap();
        assert_eq!(name, "error");
        assert_eq!(data["error"]["message"], "boom");
        assert!(!names(&events).contains(&"message_stop"));
    }

    #[test]
    fn incomplete_response_reports_max_tokens() {
        let events = run(
            &[
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "x"}),
                json!({"type": "response.incomplete", "response": {
                    "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
            ],
            NameMap::default(),
        );
        assert_eq!(
            events[events.len() - 2].1["delta"]["stop_reason"],
            "max_tokens"
        );
    }

    #[test]
    fn empty_upstream_emits_nothing() {
        assert!(run(&[], NameMap::default()).is_empty());
    }
}

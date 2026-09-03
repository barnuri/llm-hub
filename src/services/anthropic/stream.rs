//! `OpenAI` chat-completions SSE -> Anthropic Messages SSE.
//!
//! A pure state machine: bytes in, Anthropic event bytes out. It never holds
//! more than the frame currently being assembled, and it always emits a
//! well-formed stream — even when the upstream is cut mid-flight, produces
//! garbage, or never sends a terminal chunk at all.
//!
//! Two deliberate deviations from the `OpenAI` wire format, both forced by the
//! Anthropic protocol rather than chosen:
//!
//! * **Blocks are serialized.** `OpenAI` may interleave `tool_calls` indices;
//!   Anthropic's event stream cannot express that and its clients cannot
//!   represent it, so opening a block closes the one before it.
//! * **The terminal events are flushed late.** `message_delta` carries the
//!   final token counts, but `OpenAI` reports usage in a chunk that arrives
//!   *after* the one carrying `finish_reason` (that is the whole point of
//!   `stream_options.include_usage`). Emitting `message_delta` on the
//!   `finish_reason` chunk would therefore guarantee it reported zeros. The
//!   stop reason is latched and the block closed there; `message_delta` and
//!   `message_stop` flush on `[DONE]` or at end of stream — still exactly
//!   once, still last, now with real numbers.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::schemas::stop_reason::{map_stop_reason, reconcile_tool_use};
use crate::services::anthropic::response::qualify_message_id;
use crate::services::sse::{SseFrames, format_event, parse_data_frame};
use crate::services::tool_names::NameMap;

/// The `OpenAI` stream sentinel. Anthropic streams have no equivalent, so it
/// is consumed and never forwarded.
const DONE: &str = "[DONE]";
const DEFAULT_STOP_REASON: &str = "end_turn";

pub struct AnthropicStream {
    frames: SseFrames,
    served_model: String,
    /// Alias -> original tool name, undoing the request-side truncation as each
    /// `tool_use` block opens.
    names: NameMap,
    phase: Phase,
    next_index: u32,
    open: Option<OpenBlock>,
    /// `OpenAI` `tool_calls[].index` -> our accumulated state for it.
    tools: HashMap<u64, ToolCallState>,
    stop_reason: &'static str,
    /// Whether any `tool_use` block was opened, so a `finish_reason` of `stop`
    /// from an upstream that emits tool calls anyway is still reported to the
    /// client as `tool_use`.
    saw_tool_use: bool,
    /// `None` until the upstream reports usage at all.
    usage: Option<crate::services::stats::ScrapedUsage>,
}

/// Where the translated message is in its lifecycle. Every transition is
/// one-way, which is what makes "exactly one `message_start`" and "exactly one
/// `message_stop`" structural rather than a pair of flags to keep in step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing parseable seen yet; `message_start` has not been emitted.
    Pending,
    /// Content is flowing.
    Open,
    /// A `finish_reason` arrived: blocks are closed and further content is
    /// ignored, but usage-only chunks are still absorbed.
    Stopping,
    /// `message_delta` + `message_stop` have gone out; nothing follows.
    Finished,
}

#[derive(Clone, Copy)]
enum OpenBlock {
    Text(u32),
    Tool { index: u32, openai_index: u64 },
}

impl OpenBlock {
    fn index(self) -> u32 {
        match self {
            Self::Text(index) | Self::Tool { index, .. } => index,
        }
    }
}

#[derive(Default)]
struct ToolCallState {
    id: String,
    name: String,
    /// The Anthropic content-block index, once the block has been opened.
    block_index: Option<u32>,
    /// Argument fragments that arrived before the tool's name did.
    pending_args: String,
}

impl AnthropicStream {
    /// `served_model` is the qualified `<profile>/<model>` actually used, so a
    /// client that fell over to a fallback sees it in `message_start` and not
    /// only in `x-llm-hub-model`. `names` is the request's tool-name mapping,
    /// empty unless a name had to be truncated on the way out.
    #[must_use]
    pub fn new(served_model: String, names: NameMap) -> Self {
        Self {
            frames: SseFrames::new(),
            served_model,
            names,
            phase: Phase::Pending,
            next_index: 0,
            open: None,
            tools: HashMap::new(),
            stop_reason: DEFAULT_STOP_REASON,
            saw_tool_use: false,
            usage: None,
        }
    }

    /// Feeds raw upstream bytes and returns the Anthropic events they produced.
    /// An empty return is normal: a chunk may complete no frame, or a frame may
    /// carry nothing a client needs to hear about.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = String::new();
        for frame in self.frames.push(bytes) {
            let Some(payload) = parse_data_frame(&frame) else {
                continue;
            };
            let payload = payload.trim();
            if payload == DONE {
                self.emit_terminal(&mut out);
                continue;
            }
            match serde_json::from_str::<Value>(payload) {
                Ok(chunk) => self.handle_chunk(&chunk, &mut out),
                // A parse failure never aborts the stream.
                Err(e) => tracing::debug!("ignoring unparseable upstream sse frame: {e}"),
            }
        }
        out.into_bytes()
    }

    /// End of upstream. Closes any open block and emits the terminal events if
    /// they have not already gone out. Idempotent, and silent when the upstream
    /// never produced a parseable chunk — the translator does not fabricate a
    /// message it never saw begin.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        self.emit_terminal(&mut out);
        out.into_bytes()
    }

    /// Token counts scraped from the upstream chunks, for stats. `None` when
    /// the upstream never sent a `usage` object.
    #[must_use]
    pub fn observed_usage(&self) -> Option<crate::services::stats::ScrapedUsage> {
        self.usage
    }

    fn handle_chunk(&mut self, chunk: &Value, out: &mut String) {
        if self.phase == Phase::Finished {
            return;
        }
        if self.phase == Phase::Pending {
            self.emit_message_start(chunk, out);
            self.phase = Phase::Open;
        }
        self.absorb_usage(chunk);

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };

        if self.phase == Phase::Open
            && let Some(delta) = choice.get("delta")
        {
            self.handle_text(delta, out);
            self.handle_tool_calls(delta, out);
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = map_stop_reason(Some(reason));
            self.close_block(out);
            self.phase = Phase::Stopping;
        }
    }

    fn emit_message_start(&mut self, chunk: &Value, out: &mut String) {
        let id = qualify_message_id(chunk.get("id").and_then(Value::as_str).unwrap_or_default());
        // `input_tokens` is 0 here by construction: OpenAI only reports usage in
        // its terminal chunk, so the real numbers can only reach `message_delta`.
        out.push_str(&format_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": id,
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
    }

    fn absorb_usage(&mut self, chunk: &Value) {
        let Some(usage) = chunk.get("usage").filter(|value| value.is_object()) else {
            return;
        };
        let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        let cached_details = usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.usage = Some(crate::services::stats::ScrapedUsage {
            tokens_in: field("prompt_tokens"),
            tokens_out: field("completion_tokens"),
            cache_read_tokens: cached_details.max(field("cache_read_input_tokens")),
            cache_write_tokens: field("cache_creation_input_tokens"),
        });
    }

    fn handle_text(&mut self, delta: &Value, out: &mut String) {
        let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let index = match self.open {
            Some(OpenBlock::Text(index)) => index,
            _ => self.open_text_block(out),
        };
        out.push_str(&format_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text},
            }),
        ));
    }

    fn handle_tool_calls(&mut self, delta: &Value, out: &mut String) {
        let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return;
        };
        for call in calls {
            let key = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let function = call.get("function");
            let id = str_field(call.get("id"));
            let name = str_field(function.and_then(|f| f.get("name")));
            let arguments = str_field(function.and_then(|f| f.get("arguments")));

            let state = self.tools.entry(key).or_default();
            if state.id.is_empty() && !id.is_empty() {
                state.id = id.to_string();
            }
            if state.name.is_empty() && !name.is_empty() {
                state.name = name.to_string();
            }
            let block_index = state.block_index;
            let named = !state.name.is_empty();

            // A `tool_use` block needs a name; a fragment carrying only an id
            // and arguments is queued until the name shows up.
            if block_index.is_none() && named {
                self.open_tool_block(key, out);
            }
            if arguments.is_empty() {
                continue;
            }
            let opened = self.tools.get(&key).and_then(|state| state.block_index);
            match opened {
                Some(index) if self.is_open_tool(key) => out.push_str(&format_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        // Verbatim: a fragment is a partial JSON string by
                        // construction and must never be re-parsed.
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    }),
                )),
                // The block was closed by a later one: Anthropic cannot reopen
                // it, so an interleaved fragment has nowhere to go.
                Some(_) => tracing::debug!("dropping tool argument fragment for a closed block"),
                None => {
                    if let Some(state) = self.tools.get_mut(&key) {
                        state.pending_args.push_str(arguments);
                    }
                }
            }
        }
    }

    fn open_text_block(&mut self, out: &mut String) -> u32 {
        self.close_block(out);
        let index = self.take_index();
        self.open = Some(OpenBlock::Text(index));
        out.push_str(&format_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "text", "text": ""},
            }),
        ));
        index
    }

    fn open_tool_block(&mut self, key: u64, out: &mut String) {
        if !self.tools.contains_key(&key) {
            return;
        }
        self.close_block(out);
        let index = self.take_index();
        let (id, name, pending) = {
            let Some(state) = self.tools.get_mut(&key) else {
                return;
            };
            state.block_index = Some(index);
            (
                state.id.clone(),
                state.name.clone(),
                std::mem::take(&mut state.pending_args),
            )
        };
        self.open = Some(OpenBlock::Tool {
            index,
            openai_index: key,
        });
        self.saw_tool_use = true;
        // The name is the one place in the whole translated stream where a
        // truncated tool name surfaces, so restoring it here is enough.
        let name = self.names.restore_name(&name);
        out.push_str(&format_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}},
            }),
        ));
        if !pending.is_empty() {
            out.push_str(&format_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": pending},
                }),
            ));
        }
    }

    fn close_block(&mut self, out: &mut String) {
        let Some(block) = self.open.take() else {
            return;
        };
        out.push_str(&format_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": block.index()}),
        ));
    }

    fn emit_terminal(&mut self, out: &mut String) {
        // Pending means the upstream never produced a parseable chunk: there is
        // no message to close, and one is never fabricated.
        if matches!(self.phase, Phase::Pending | Phase::Finished) {
            return;
        }
        self.close_block(out);
        let usage = self.usage.unwrap_or_default();
        let (input_tokens, output_tokens) = (usage.tokens_in, usage.tokens_out);
        let stop_reason = reconcile_tool_use(self.stop_reason, self.saw_tool_use);
        out.push_str(&format_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": Value::Null},
                "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
            }),
        ));
        out.push_str(&format_event(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
        self.phase = Phase::Finished;
    }

    fn is_open_tool(&self, key: u64) -> bool {
        matches!(self.open, Some(OpenBlock::Tool { openai_index, .. }) if openai_index == key)
    }

    fn take_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

fn str_field(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLE: &str =
        r#"{"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#;
    const TEXT_A: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"he"}}]}"#;
    const TEXT_B: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"llo"}}]}"#;
    const STOP: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;
    const TOOL_STOP: &str =
        r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;
    const USAGE: &str =
        r#"{"id":"c1","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":6}}"#;
    const TOOL_HEAD: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"search","arguments":""}}]}}]}"#;
    const TOOL_ARGS: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":"}}]}}]}"#;
    const TOOL_ARGS_TAIL: &str = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"rust\"}"}}]}}]}"#;

    /// Parses emitted SSE bytes back into `(event name, payload)` pairs.
    fn parse(bytes: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        text.split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let mut name = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        name = rest.to_string();
                    }
                    if let Some(rest) = line.strip_prefix("data: ") {
                        data = rest.to_string();
                    }
                }
                (name, serde_json::from_str(&data).unwrap())
            })
            .collect()
    }

    fn feed(stream: &mut AnthropicStream, payloads: &[&str]) -> Vec<(String, Value)> {
        let mut out: Vec<u8> = Vec::new();
        for payload in payloads {
            out.extend_from_slice(&stream.push(format!("data: {payload}\n\n").as_bytes()));
        }
        parse(&out)
    }

    fn run(payloads: &[&str]) -> (AnthropicStream, Vec<(String, Value)>) {
        let mut stream = AnthropicStream::new("p/m".to_string(), NameMap::default());
        let events = feed(&mut stream, payloads);
        (stream, events)
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn count(events: &[(String, Value)], name: &str) -> usize {
        events.iter().filter(|(event, _)| event == name).count()
    }

    #[test]
    fn emits_message_start_exactly_once_on_first_chunk() {
        let (_, events) = run(&[ROLE, TEXT_A, TEXT_B]);
        assert_eq!(count(&events, "message_start"), 1);
        assert_eq!(events[0].0, "message_start");
        let message = &events[0].1["message"];
        assert_eq!(message["id"], "msg_c1");
        assert_eq!(message["model"], "p/m");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"], json!([]));
        assert_eq!(message["stop_reason"], Value::Null);
        assert_eq!(message["usage"]["input_tokens"], 0);
    }

    #[test]
    fn text_only_stream_emits_full_canonical_sequence() {
        let (_, events) = run(&[ROLE, TEXT_A, TEXT_B, STOP, USAGE, "[DONE]"]);
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            events[2].1["delta"],
            json!({"type": "text_delta", "text": "he"})
        );
        assert_eq!(
            events[3].1["delta"],
            json!({"type": "text_delta", "text": "llo"})
        );
        assert_eq!(events[4].1["index"], 0);
    }

    #[test]
    fn content_block_start_precedes_every_delta_for_that_index() {
        let (_, events) = run(&[TEXT_A, TOOL_HEAD, TOOL_ARGS, TOOL_STOP, "[DONE]"]);
        let mut opened: Vec<u64> = Vec::new();
        for (name, data) in &events {
            let index = data["index"].as_u64();
            match name.as_str() {
                "content_block_start" => opened.push(index.unwrap()),
                "content_block_delta" | "content_block_stop" => {
                    assert!(
                        opened.contains(&index.unwrap()),
                        "{name} for an unopened index: {data}"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(opened, vec![0, 1]);
    }

    #[test]
    fn tool_call_stream_emits_tool_use_start_then_input_json_deltas() {
        let (_, events) = run(&[TOOL_HEAD, TOOL_ARGS, TOOL_ARGS_TAIL, TOOL_STOP, "[DONE]"]);
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "tool_use", "id": "call_1", "name": "search", "input": {}})
        );
        assert_eq!(events[5].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn arguments_fragments_are_forwarded_verbatim_without_reparsing() {
        let (_, events) = run(&[TOOL_HEAD, TOOL_ARGS, TOOL_ARGS_TAIL]);
        let fragments: Vec<&str> = events
            .iter()
            .filter(|(name, _)| name == "content_block_delta")
            .map(|(_, data)| data["delta"]["partial_json"].as_str().unwrap())
            .collect();
        assert_eq!(fragments, vec!["{\"q\":", "\"rust\"}"]);
    }

    #[test]
    fn args_arriving_before_name_are_queued_and_flushed_after_start() {
        let headless = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"arguments":"{\"a\":"}}]}}]}"#;
        let named = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"late","arguments":"1}"}}]}}]}"#;
        let (_, events) = run(&[headless, named]);
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
            ]
        );
        assert_eq!(events[1].1["content_block"]["name"], "late");
        assert_eq!(events[1].1["content_block"]["id"], "call_9");
        assert_eq!(events[2].1["delta"]["partial_json"], "{\"a\":");
        assert_eq!(events[3].1["delta"]["partial_json"], "1}");
    }

    #[test]
    fn second_tool_call_closes_the_first_block_before_opening() {
        let second = r#"{"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"other","arguments":"{}"}}]}}]}"#;
        let (_, events) = run(&[TOOL_HEAD, second]);
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
            ]
        );
        assert_eq!(events[2].1["index"], 0);
        assert_eq!(events[3].1["index"], 1);
        assert_eq!(events[3].1["content_block"]["name"], "other");
    }

    #[test]
    fn text_then_tool_call_assigns_indices_zero_then_one() {
        let (_, events) = run(&[TEXT_A, TOOL_HEAD]);
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[1].1["content_block"]["type"], "text");
        assert_eq!(events[3].1["index"], 0);
        assert_eq!(events[3].0, "content_block_stop");
        assert_eq!(events[4].1["index"], 1);
        assert_eq!(events[4].1["content_block"]["type"], "tool_use");
    }

    #[test]
    fn finish_reason_emits_message_delta_once_with_usage() {
        let (mut stream, mut events) = run(&[TEXT_A, STOP, USAGE, "[DONE]"]);
        events.extend(parse(&stream.finish()));
        assert_eq!(count(&events, "message_delta"), 1);
        assert_eq!(count(&events, "message_stop"), 1);
        let delta = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        assert_eq!(
            delta.1["delta"],
            json!({"stop_reason": "end_turn", "stop_sequence": null})
        );
        assert_eq!(
            delta.1["usage"],
            json!({"input_tokens": 5, "output_tokens": 6})
        );
    }

    #[test]
    fn done_sentinel_is_consumed_not_forwarded() {
        let (_, events) = run(&[TEXT_A, STOP, "[DONE]"]);
        assert!(
            events
                .iter()
                .all(|(name, data)| name != "[DONE]" && data.as_str() != Some("[DONE]")),
            "sentinel leaked: {events:?}"
        );
        assert_eq!(names(&events).last(), Some(&"message_stop"));
    }

    #[test]
    fn unparseable_frame_is_ignored_and_stream_continues() {
        let (_, events) = run(&[TEXT_A, "{not json", TEXT_B]);
        assert_eq!(count(&events, "content_block_delta"), 2);
        assert_eq!(count(&events, "message_start"), 1);
    }

    #[test]
    fn empty_upstream_emits_nothing() {
        let mut stream = AnthropicStream::new("p/m".to_string(), NameMap::default());
        assert!(stream.push(b"data: [DONE]\n\n").is_empty());
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn truncated_upstream_still_closes_block_and_emits_delta_and_stop() {
        let (mut stream, mut events) = run(&[TOOL_HEAD, TOOL_ARGS]);
        events.extend(parse(&stream.finish()));
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        // The message carries a tool_use block, so the stop reason is tool_use
        // even though the upstream never sent a finish_reason at all.
        assert_eq!(events[4].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn a_tool_call_with_a_stop_finish_reason_still_reports_tool_use() {
        let (mut stream, mut events) = run(&[TOOL_HEAD, TOOL_ARGS, STOP]);
        events.extend(parse(&stream.finish()));
        let delta = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn a_text_only_stream_is_unaffected_by_the_tool_use_reconcile() {
        let (mut stream, mut events) = run(&[TEXT_A, STOP]);
        events.extend(parse(&stream.finish()));
        let delta = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        assert_eq!(delta.1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn finish_is_idempotent_and_never_double_emits_message_stop() {
        let (mut stream, mut events) = run(&[TEXT_A]);
        events.extend(parse(&stream.finish()));
        assert!(stream.finish().is_empty());
        assert!(stream.push(b"data: [DONE]\n\n").is_empty());
        assert_eq!(count(&events, "message_stop"), 1);
        assert_eq!(count(&events, "message_delta"), 1);
    }

    #[test]
    fn observed_usage_is_reported_for_stats() {
        let (stream, _) = run(&[TEXT_A]);
        assert!(stream.observed_usage().is_none());
        let (stream, _) = run(&[TEXT_A, USAGE, "[DONE]"]);
        let usage = stream
            .observed_usage()
            .expect("usage chunk should be absorbed");
        assert_eq!(usage.tokens_in, 5);
        assert_eq!(usage.tokens_out, 6);
    }

    #[test]
    fn frames_split_across_chunk_boundaries_are_reassembled() {
        let mut stream = AnthropicStream::new("p/m".to_string(), NameMap::default());
        let raw = format!("data: {TEXT_A}\n\n");
        let (head, tail) = raw.split_at(raw.len() / 2);
        assert!(stream.push(head.as_bytes()).is_empty());
        let events = parse(&stream.push(tail.as_bytes()));
        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
    }
}

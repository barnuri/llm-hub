use bytes::Bytes;
use serde_json::{Value, json};

use crate::schemas::app_error::AppError;
use crate::schemas::reply_protocol::ReplyProtocol;
use crate::services::sse::format_event;

/// Re-emits one finished reply as well-formed SSE, so a buffered answer reads like a live stream.
#[derive(Debug)]
pub struct BufferedReplyStream;

impl BufferedReplyStream {
    const DONE_FRAME: &'static str = "data: [DONE]\n\n";
    const CHUNK_OBJECT: &'static str = "chat.completion.chunk";
    const FALLBACK_ID: &'static str = "chatcmpl-llm-hub";

    /// Re-emits a final chat completion or Anthropic message as SSE in `protocol`, keeping ids, finish, usage and reasoning.
    pub fn render(protocol: ReplyProtocol, reply: &Value) -> Result<Bytes, AppError> {
        let text = match protocol {
            ReplyProtocol::OpenAiChat => Self::openai_frames(reply)?,
            ReplyProtocol::AnthropicMessage => Self::anthropic_frames(reply)?,
        };
        Ok(Bytes::from(text))
    }

    fn openai_frames(reply: &Value) -> Result<String, AppError> {
        let choice = reply
            .pointer("/choices/0")
            .ok_or_else(|| Self::not_a_reply("chat completion", "choices"))?;
        let message = choice
            .get("message")
            .ok_or_else(|| Self::not_a_reply("chat completion", "message"))?;
        let id = reply
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(Self::FALLBACK_ID);
        let model = reply
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let created = reply.get("created").cloned().unwrap_or(json!(0));
        let chunk = |delta: Value, finish: Value| {
            json!({
                "id": id,
                "object": Self::CHUNK_OBJECT,
                "created": created.clone(),
                "model": model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        };

        let mut frames = vec![chunk(json!({ "role": "assistant" }), Value::Null)];
        if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            frames.push(chunk(
                json!({ "reasoning_content": reasoning }),
                Value::Null,
            ));
        }
        if let Some(content) = message.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            frames.push(chunk(json!({ "content": content }), Value::Null));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (index, call) in calls.iter().enumerate() {
                frames.push(chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "id": call.get("id").cloned().unwrap_or(Value::Null),
                        "type": "function",
                        "function": call.get("function").cloned().unwrap_or(json!({})),
                    }] }),
                    Value::Null,
                ));
            }
        }
        let finish = choice.get("finish_reason").cloned().unwrap_or(Value::Null);
        frames.push(chunk(json!({}), finish));
        if let Some(usage) = reply.get("usage").filter(|usage| usage.is_object()) {
            frames.push(json!({
                "id": id,
                "object": Self::CHUNK_OBJECT,
                "created": created.clone(),
                "model": model,
                "choices": [],
                "usage": usage,
            }));
        }

        let mut out = String::new();
        for frame in frames {
            out.push_str("data: ");
            out.push_str(&frame.to_string());
            out.push_str("\n\n");
        }
        out.push_str(Self::DONE_FRAME);
        Ok(out)
    }

    fn anthropic_frames(reply: &Value) -> Result<String, AppError> {
        let content = reply
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| Self::not_a_reply("Anthropic message", "content"))?;
        let usage = reply.get("usage").cloned().unwrap_or_else(|| json!({}));
        let mut start_usage = usage.clone();
        if let Some(object) = start_usage.as_object_mut() {
            object.insert("output_tokens".to_owned(), json!(0));
        }
        let mut events: Vec<(&'static str, Value)> = vec![(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": reply.get("id").cloned().unwrap_or(Value::Null),
                    "type": "message",
                    "role": "assistant",
                    "model": reply.get("model").cloned().unwrap_or(Value::Null),
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": start_usage,
                },
            }),
        )];
        for (index, block) in content.iter().enumerate() {
            Self::push_block_events(&mut events, index, block);
        }
        events.push((
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": reply.get("stop_reason").cloned().unwrap_or(Value::Null),
                    "stop_sequence": Value::Null,
                },
                "usage": { "output_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)) },
            }),
        ));
        events.push(("message_stop", json!({ "type": "message_stop" })));

        let out: String = events
            .iter()
            .map(|(name, data)| format_event(name, data))
            .collect();
        Ok(out)
    }

    /// The start, deltas and stop of one content block. Reasoning keeps its signature.
    fn push_block_events(events: &mut Vec<(&'static str, Value)>, index: usize, block: &Value) {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (start, deltas): (Value, Vec<Value>) = match kind {
            "thinking" => {
                let thinking = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut deltas = vec![json!({ "type": "thinking_delta", "thinking": thinking })];
                deltas.push(json!({ "type": "signature_delta", "signature": signature }));
                (
                    json!({ "type": "thinking", "thinking": "", "signature": "" }),
                    deltas,
                )
            }
            "text" => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                (
                    json!({ "type": "text", "text": "" }),
                    vec![json!({ "type": "text_delta", "text": text })],
                )
            }
            "tool_use" => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                (
                    json!({
                        "type": "tool_use",
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "name": block.get("name").cloned().unwrap_or(Value::Null),
                        "input": {},
                    }),
                    vec![json!({ "type": "input_json_delta", "partial_json": input.to_string() })],
                )
            }
            _ => (block.clone(), Vec::new()),
        };
        events.push((
            "content_block_start",
            json!({ "type": "content_block_start", "index": index, "content_block": start }),
        ));
        for delta in deltas {
            // A delta whose every payload field is empty carries nothing, so it is not sent.
            let carries_nothing = delta.as_object().is_some_and(|object| {
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "type")
                    .all(|(_, value)| value.as_str().is_some_and(str::is_empty))
            });
            if !carries_nothing {
                events.push((
                    "content_block_delta",
                    json!({ "type": "content_block_delta", "index": index, "delta": delta }),
                ));
            }
        }
        events.push((
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        ));
    }

    fn not_a_reply(shape: &str, missing: &str) -> AppError {
        AppError::UpstreamUnavailable(format!("review reply is not a {shape}: no {missing}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders replies and parses their SSE text for the golden and shape tests.
    struct Fixture;

    impl Fixture {
        /// Parses SSE text into (event name, data) pairs; `OpenAI` frames have no event line.
        fn parse_sse(text: &str) -> Vec<(Option<String>, String)> {
            text.split("\n\n")
                .filter(|frame| !frame.is_empty())
                .map(|frame| {
                    let mut event = None;
                    let mut data = String::new();
                    for line in frame.lines() {
                        if let Some(name) = line.strip_prefix("event: ") {
                            event = Some(name.to_owned());
                        } else if let Some(payload) = line.strip_prefix("data: ") {
                            data = payload.to_owned();
                        }
                    }
                    (event, data)
                })
                .collect()
        }

        fn render_text(protocol: ReplyProtocol, reply: &Value) -> String {
            String::from_utf8(
                BufferedReplyStream::render(protocol, reply)
                    .unwrap()
                    .to_vec(),
            )
            .unwrap()
        }

        fn completion() -> Value {
            json!({
                "id": "chatcmpl-42",
                "object": "chat.completion",
                "created": 1_700_000_000u64,
                "model": "copilot/gpt-5",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "The sum is 5.",
                        "reasoning_content": "Add 2 and 3."
                    },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 20, "completion_tokens": 6, "total_tokens": 26 }
            })
        }
    }

    #[test]
    fn openai_golden_frames_keep_id_finish_reason_usage_and_reasoning() {
        let text = Fixture::render_text(ReplyProtocol::OpenAiChat, &Fixture::completion());
        let frames = Fixture::parse_sse(&text);
        assert!(text.ends_with("data: [DONE]\n\n"));
        let values: Vec<Value> = frames
            .iter()
            .filter(|(_, data)| data.as_str() != "[DONE]")
            .map(|(_, data)| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(
            values[0]["choices"][0]["delta"],
            json!({ "role": "assistant" })
        );
        assert_eq!(
            values[1]["choices"][0]["delta"],
            json!({ "reasoning_content": "Add 2 and 3." })
        );
        assert_eq!(
            values[2]["choices"][0]["delta"],
            json!({ "content": "The sum is 5." })
        );
        let last_choice = values.len() - 2;
        assert_eq!(values[last_choice]["choices"][0]["finish_reason"], "stop");
        let usage = values.last().unwrap();
        assert_eq!(usage["usage"]["total_tokens"], 26);
        assert_eq!(usage["choices"], json!([]));
        for value in &values {
            assert_eq!(value["id"], "chatcmpl-42");
            assert_eq!(value["object"], "chat.completion.chunk");
        }
    }

    #[test]
    fn openai_tool_calls_are_streamed_as_deltas() {
        let reply = json!({
            "id": "chatcmpl-7",
            "model": "copilot/gpt-5",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{ "id": "call_1", "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" } }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let text = Fixture::render_text(ReplyProtocol::OpenAiChat, &reply);
        assert!(text.contains("\"name\":\"read_file\""));
        assert!(text.contains("\"finish_reason\":\"tool_calls\""));
    }

    #[test]
    fn anthropic_golden_events_keep_ids_stop_reason_usage_and_thinking() {
        let message = json!({
            "id": "msg_9",
            "type": "message",
            "role": "assistant",
            "model": "copilot/claude-sonnet",
            "content": [
                { "type": "thinking", "thinking": "Add 2 and 3.", "signature": "sig-1" },
                { "type": "text", "text": "The sum is 5." }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 20, "output_tokens": 6 }
        });
        let text = Fixture::render_text(ReplyProtocol::AnthropicMessage, &message);
        let events: Vec<(String, Value)> = Fixture::parse_sse(&text)
            .into_iter()
            .map(|(event, data)| (event.unwrap(), serde_json::from_str(&data).unwrap()))
            .collect();
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0].1["message"]["id"], "msg_9");
        assert_eq!(events[0].1["message"]["usage"]["output_tokens"], 0);
        assert_eq!(events[2].1["delta"]["type"], "thinking_delta");
        assert_eq!(events[3].1["delta"]["signature"], "sig-1");
        assert_eq!(events[6].1["delta"]["text"], "The sum is 5.");
        assert_eq!(events[8].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[8].1["usage"]["output_tokens"], 6);
    }

    #[test]
    fn a_tool_use_block_streams_its_input_as_json() {
        let message = json!({
            "id": "msg_10",
            "model": "copilot/claude-sonnet",
            "content": [{ "type": "tool_use", "id": "toolu_1", "name": "grep", "input": { "q": "x" } }],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 1, "output_tokens": 2 }
        });
        let text = Fixture::render_text(ReplyProtocol::AnthropicMessage, &message);
        assert!(text.contains("\"partial_json\":\"{\\\"q\\\":\\\"x\\\"}\""));
        assert!(text.contains("\"name\":\"grep\""));
    }

    #[test]
    fn a_body_without_choices_or_content_is_an_error() {
        assert!(
            BufferedReplyStream::render(ReplyProtocol::OpenAiChat, &json!({"error": 1})).is_err()
        );
        assert!(
            BufferedReplyStream::render(ReplyProtocol::AnthropicMessage, &json!({"id": "x"}))
                .is_err()
        );
    }
}

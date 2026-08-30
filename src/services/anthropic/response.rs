//! `OpenAI` chat completion -> Anthropic Messages response.
//!
//! Fail-open on the success path: a malformed `tool_calls[].function.arguments`
//! string degrades to an empty `input` object rather than turning a 200 into a
//! 502. Only a body that will not parse at all is rendered as an error.

use serde_json::{Value, json};

use crate::consts::ANTHROPIC_MESSAGE_ID_PREFIX;
use crate::schemas::anthropic_error::error_type_for_status;
use crate::schemas::stop_reason::{map_stop_reason, reconcile_tool_use};

/// Renders an upstream chat-completions response as an Anthropic message (or
/// an Anthropic error envelope for a `>= 400` status). `served_model` is the
/// qualified `<profile>/<model>` actually used, matching `x-llm-hub-model` — so
/// a client that fell over to a fallback can see it in the body too.
#[must_use]
pub fn to_anthropic(status: u16, body: &[u8], served_model: &str) -> Vec<u8> {
    let value = if status >= 400 {
        error_envelope(status, body)
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(completion) => success_message(&completion, served_model),
            Err(e) => error_envelope_with_message(
                502,
                &format!("upstream returned a non-JSON success body: {e}"),
            ),
        }
    };
    to_bytes(&value)
}

/// A standalone Anthropic error envelope, for failures the hub itself detects
/// while handling the response (over-cap bodies, read errors).
#[must_use]
pub fn error_body(status: u16, message: &str) -> Vec<u8> {
    to_bytes(&error_envelope_with_message(status, message))
}

fn success_message(completion: &Value, served_model: &str) -> Value {
    let choice = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice.and_then(|choice| choice.get("message"));

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    if let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        content.extend(calls.iter().map(tool_use_block));
    }

    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str);
    // An upstream that reports `stop` while emitting tool calls would otherwise
    // end the client's agent loop before the tool ever runs.
    let saw_tool_use = content
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"));
    let stop_reason = reconcile_tool_use(map_stop_reason(finish_reason), saw_tool_use);
    let (input_tokens, output_tokens) = usage(completion);

    json!({
        "id": qualify_message_id(completion.get("id").and_then(Value::as_str).unwrap_or_default()),
        "type": "message",
        "role": "assistant",
        "model": served_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    })
}

fn tool_use_block(call: &Value) -> Value {
    let function = call.get("function");
    let arguments = function
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input = serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name": function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "input": input,
    })
}

/// Anthropic message ids are `msg_`-prefixed; upstream chat-completion ids are
/// not. Shared with the streaming translator, which qualifies the id it reads
/// off the first chunk of the stream.
pub(crate) fn qualify_message_id(raw: &str) -> String {
    if raw.is_empty() {
        return format!("{ANTHROPIC_MESSAGE_ID_PREFIX}unknown");
    }
    if raw.starts_with(ANTHROPIC_MESSAGE_ID_PREFIX) {
        return raw.to_string();
    }
    format!("{ANTHROPIC_MESSAGE_ID_PREFIX}{raw}")
}

fn usage(completion: &Value) -> (u64, u64) {
    let usage = completion.get("usage");
    let field = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    (field("prompt_tokens"), field("completion_tokens"))
}

fn error_envelope(status: u16, body: &[u8]) -> Value {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let message = parsed
        .as_ref()
        .and_then(upstream_message)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    error_envelope_with_message(status, &message)
}

fn error_envelope_with_message(status: u16, message: &str) -> Value {
    json!({
        "type": "error",
        "error": { "type": error_type_for_status(status), "message": message },
    })
}

fn upstream_message(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_string)
}

fn to_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| {
        br#"{"type":"error","error":{"type":"api_error","message":"response serialization failed"}}"#
            .to_vec()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(completion: &Value) -> Value {
        let bytes = to_anthropic(200, &serde_json::to_vec(completion).unwrap(), "p/m");
        serde_json::from_slice(&bytes).unwrap()
    }

    fn completion(message: &Value, finish_reason: &str) -> Value {
        json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "message": message.clone(), "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 22}
        })
    }

    #[test]
    fn text_only_completion_becomes_single_text_block() {
        let out = translate(&completion(
            &json!({"role": "assistant", "content": "hello"}),
            "stop",
        ));
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["content"], json!([{"type": "text", "text": "hello"}]));
        assert_eq!(out["stop_sequence"], Value::Null);
    }

    #[test]
    fn tool_calls_become_tool_use_blocks_with_parsed_input() {
        let out = translate(&completion(
            &json!({"role": "assistant", "content": Value::Null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}}
            ]}),
            "tool_calls",
        ));
        assert_eq!(
            out["content"],
            json!([{"type": "tool_use", "id": "call_1", "name": "search", "input": {"q": "rust"}}])
        );
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn tool_calls_with_a_stop_finish_reason_still_report_tool_use() {
        // Several OpenAI-compatible upstreams emit `stop` alongside tool calls;
        // reporting `end_turn` would end the client's agent loop before the
        // tool ran.
        let out = translate(&completion(
            &json!({"role": "assistant", "content": Value::Null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "search", "arguments": "{}"}}
            ]}),
            "stop",
        ));
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["type"], "tool_use");
    }

    #[test]
    fn a_truncated_tool_call_still_reports_max_tokens() {
        let out = translate(&completion(
            &json!({"role": "assistant", "content": Value::Null, "tool_calls": [
                {"id": "call_1", "type": "function",
                 "function": {"name": "search", "arguments": "{}"}}
            ]}),
            "length",
        ));
        assert_eq!(out["stop_reason"], "max_tokens");
    }

    #[test]
    fn text_and_tool_calls_keep_text_first() {
        let out = translate(&completion(
            &json!({"role": "assistant", "content": "thinking", "tool_calls": [
                {"id": "c", "function": {"name": "n", "arguments": "{}"}}
            ]}),
            "tool_calls",
        ));
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][1]["type"], "tool_use");
    }

    #[test]
    fn malformed_tool_arguments_degrade_to_empty_object() {
        let out = translate(&completion(
            &json!({"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": "n", "arguments": "{\"q\": "}}
            ]}),
            "tool_calls",
        ));
        assert_eq!(out["content"][0]["input"], json!({}));
    }

    #[test]
    fn empty_content_emits_no_text_block() {
        let out = translate(&completion(
            &json!({"role": "assistant", "content": ""}),
            "stop",
        ));
        assert_eq!(out["content"], json!([]));
        let out = translate(&completion(
            &json!({"role": "assistant", "content": Value::Null}),
            "stop",
        ));
        assert_eq!(out["content"], json!([]));
    }

    #[test]
    fn finish_reason_maps_stop_length_and_tool_calls() {
        let reason = |finish: &str| {
            translate(&completion(&json!({"content": "x"}), finish))["stop_reason"].clone()
        };
        assert_eq!(reason("stop"), "end_turn");
        assert_eq!(reason("length"), "max_tokens");
        assert_eq!(reason("tool_calls"), "tool_use");
    }

    #[test]
    fn unknown_finish_reason_defaults_to_end_turn() {
        let out = translate(&json!({"id": "x", "choices": [{"message": {"content": "a"}}]}));
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn usage_maps_prompt_and_completion_to_input_and_output() {
        let out = translate(&completion(&json!({"content": "x"}), "stop"));
        assert_eq!(
            out["usage"],
            json!({"input_tokens": 11, "output_tokens": 22})
        );
    }

    #[test]
    fn missing_usage_becomes_zeros() {
        let out = translate(&json!({"id": "x", "choices": []}));
        assert_eq!(out["usage"], json!({"input_tokens": 0, "output_tokens": 0}));
        assert_eq!(out["content"], json!([]));
    }

    #[test]
    fn id_is_prefixed_msg_when_upstream_id_is_not() {
        let out = translate(&completion(&json!({"content": "x"}), "stop"));
        assert_eq!(out["id"], "msg_chatcmpl-1");
        let already = translate(&json!({"id": "msg_abc", "choices": []}));
        assert_eq!(already["id"], "msg_abc");
        let missing = translate(&json!({"choices": []}));
        assert_eq!(missing["id"], "msg_unknown");
    }

    #[test]
    fn model_reports_the_qualified_served_id() {
        let bytes = to_anthropic(200, br#"{"choices":[]}"#, "groq/llama-4");
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["model"], "groq/llama-4");
    }

    #[test]
    fn openai_error_body_becomes_anthropic_error_envelope() {
        let bytes = to_anthropic(
            429,
            br#"{"error":{"message":"slow down","type":"rate_limit_exceeded"}}"#,
            "p/m",
        );
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            out,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
        );
    }

    #[test]
    fn non_json_error_body_becomes_message_verbatim() {
        let bytes = to_anthropic(502, b"  upstream exploded\n", "p/m");
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "upstream exploded");
    }

    #[test]
    fn non_json_success_body_becomes_an_error_envelope() {
        let bytes = to_anthropic(200, b"not json", "p/m");
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn error_body_renders_a_standalone_envelope() {
        let out: Value = serde_json::from_slice(&error_body(413, "too big")).unwrap();
        assert_eq!(
            out,
            json!({"type": "error", "error": {"type": "request_too_large", "message": "too big"}})
        );
    }
}

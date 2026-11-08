//! The answer text inside a captured upstream reply. The reader runs when a trace is viewed, never on the request path.

use serde_json::Value;

use crate::services::sse::{SseFrames, parse_data_frame};

/// The text a captured reply carries, joined in order. Reads the deltas of a streamed reply (chat completions,
/// Anthropic messages, Responses) or the text parts of a whole JSON body. Returns an empty string when none is found.
#[must_use]
pub fn answer_text(raw: &[u8]) -> String {
    let mut frames = SseFrames::new();
    let mut out = String::new();
    let mut streamed = false;
    for frame in frames.push(raw) {
        let Some(data) = parse_data_frame(&frame) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        streamed = true;
        push_delta(&value, &mut out);
    }
    if !streamed && let Ok(value) = serde_json::from_slice::<Value>(raw) {
        push_whole(&value, &mut out);
    }
    out
}

/// Appends the text one streamed event adds.
fn push_delta(event: &Value, out: &mut String) {
    let text = match event.get("type").and_then(Value::as_str) {
        Some("content_block_delta") => event.pointer("/delta/text").and_then(Value::as_str),
        Some("response.output_text.delta") => event.get("delta").and_then(Value::as_str),
        _ => event
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str),
    };
    if let Some(text) = text {
        out.push_str(text);
    }
}

/// Appends the text of a whole, non-streamed body.
fn push_whole(body: &Value, out: &mut String) {
    if let Some(text) = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    {
        out.push_str(text);
        return;
    }
    if let Some(blocks) = body.get("content").and_then(Value::as_array) {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
        return;
    }
    if let Some(items) = body.get("output").and_then(Value::as_array) {
        for item in items {
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::answer_text;

    #[test]
    fn reads_chat_completion_deltas() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
data: [DONE]\n\n";

        assert_eq!(answer_text(raw), "Hello");
    }

    #[test]
    fn reads_anthropic_text_deltas_and_skips_other_events() {
        let raw = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";

        assert_eq!(answer_text(raw), "Hi");
    }

    #[test]
    fn reads_responses_output_text_deltas() {
        let raw = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n";

        assert_eq!(answer_text(raw), "ok");
    }

    #[test]
    fn reads_whole_json_bodies() {
        let chat = br#"{"choices":[{"message":{"content":"chat"}}]}"#;
        let anthropic = br#"{"content":[{"type":"text","text":"msg"}]}"#;
        let responses = br#"{"output":[{"content":[{"type":"output_text","text":"resp"}]}]}"#;

        assert_eq!(answer_text(chat), "chat");
        assert_eq!(answer_text(anthropic), "msg");
        assert_eq!(answer_text(responses), "resp");
    }

    #[test]
    fn unreadable_bytes_give_empty_text() {
        assert_eq!(answer_text(b"not a reply"), "");
        assert_eq!(answer_text(b""), "");
    }
}

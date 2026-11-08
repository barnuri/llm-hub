//! Whole-body image detection for the request shapes that can carry an image:
//! Anthropic `messages`, `OpenAI` `chat/completions` and `OpenAI` `responses`.

use serde_json::Value;

/// Which request body shape a path carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyShape {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl BodyShape {
    pub(crate) fn from_path(path: &str) -> Option<BodyShape> {
        let path = path.trim_end_matches('/');
        if path.ends_with("chat/completions") {
            Some(BodyShape::ChatCompletions)
        } else if path.ends_with("responses") {
            Some(BodyShape::Responses)
        } else if path.ends_with("messages") {
            Some(BodyShape::AnthropicMessages)
        } else {
            None
        }
    }
}

/// Whether the request body holds an image part anywhere in its messages or input: the latest
/// turn, earlier turns, and tool results at any depth. A path with no known shape has no image.
#[must_use]
pub(crate) fn request_has_image(path: &str, body: &Value) -> bool {
    let Some(shape) = BodyShape::from_path(path) else {
        return false;
    };
    let (key, image_type) = match shape {
        BodyShape::ChatCompletions => ("messages", "image_url"),
        BodyShape::Responses => ("input", "input_image"),
        BodyShape::AnthropicMessages => ("messages", "image"),
    };
    let Some(Value::Array(items)) = body.get(key) else {
        return false;
    };
    items.iter().any(|item| holds_part(item, image_type))
}

/// Whether `value` is an `image_type` part or holds one at any depth. Strings and numbers hold
/// no parts, so encoded image data is never scanned.
fn holds_part(value: &Value, image_type: &str) -> bool {
    if part_type(value) == Some(image_type) {
        return true;
    }
    match value {
        Value::Array(items) => items.iter().any(|item| holds_part(item, image_type)),
        Value::Object(fields) => fields.values().any(|field| holds_part(field, image_type)),
        _ => false,
    }
}

fn part_type(part: &Value) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CHAT: &str = "/v1/chat/completions";
    const RESPONSES: &str = "/v1/responses";
    const MESSAGES: &str = "/v1/messages";

    #[test]
    fn image_in_an_earlier_turn_counts_for_anthropic_messages() {
        let body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
                {"type": "text", "text": "what is this"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "a cat"}]},
            {"role": "user", "content": "and the next one?"}
        ]});

        assert!(request_has_image(MESSAGES, &body));
    }

    #[test]
    fn image_in_an_earlier_turn_counts_for_openai_chat() {
        let body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]},
            {"role": "assistant", "content": "a cat"},
            {"role": "user", "content": "and the next one?"}
        ]});

        assert!(request_has_image(CHAT, &body));
    }

    #[test]
    fn image_inside_an_anthropic_tool_result_counts() {
        let body = json!({"messages": [
            {"role": "user", "content": "screenshot the page"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "shot"}]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "t1",
                "content": [{"type": "image", "source": {"type": "base64", "data": "AAAA"}}]
            }]}
        ]});

        assert!(request_has_image(MESSAGES, &body));
    }

    #[test]
    fn image_inside_an_openai_tool_message_counts() {
        let body = json!({"messages": [
            {"role": "user", "content": "screenshot the page"},
            {"role": "assistant", "tool_calls": [{"id": "c1"}]},
            {"role": "tool", "tool_call_id": "c1", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}
        ]});

        assert!(request_has_image(CHAT, &body));
    }

    #[test]
    fn image_inside_a_responses_function_call_output_counts() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "call_id": "c1", "name": "shot"},
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
            ]}
        ]});

        assert!(request_has_image(RESPONSES, &body));
    }

    #[test]
    fn text_only_body_has_no_image() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
            ]}
        ]});

        assert!(!request_has_image(MESSAGES, &body));
        assert!(!request_has_image(CHAT, &body));
    }

    #[test]
    fn image_type_inside_a_string_is_not_a_part() {
        let body = json!({"messages": [
            {"role": "user", "content": "the part is {\"type\": \"image\"}"}
        ]});

        assert!(!request_has_image(MESSAGES, &body));
    }

    #[test]
    fn image_part_under_another_shape_is_ignored() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "input_image", "image_url": "x"}]}
        ]});

        assert!(!request_has_image(CHAT, &body));
    }

    #[test]
    fn unknown_path_has_no_image() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "image", "source": {}}]}
        ]});

        assert!(!request_has_image("/v1/embeddings", &body));
    }
}

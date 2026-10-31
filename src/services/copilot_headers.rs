//! Copilot request headers for each auth flow, plus the per-request
//! `X-Initiator` and `Copilot-Vision-Request` values read from the body.

use axum::http::{HeaderName, HeaderValue};
use serde_json::Value;

use crate::configs::CopilotFlow;
use crate::consts::{
    COPILOT_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_VERSION, COPILOT_INTEGRATION_ID,
    COPILOT_OPENAI_INTENT, COPILOT_VSCODE_API_VERSION, COPILOT_VSCODE_USER_AGENT, VERSION,
};

pub const HEADER_X_INITIATOR: &str = "x-initiator";
pub const HEADER_VISION_REQUEST: &str = "copilot-vision-request";
const HEADER_EDITOR_VERSION: &str = "editor-version";
const HEADER_EDITOR_PLUGIN_VERSION: &str = "editor-plugin-version";
const HEADER_INTEGRATION_ID: &str = "copilot-integration-id";
const HEADER_OPENAI_INTENT: &str = "openai-intent";
const HEADER_API_VERSION: &str = "x-github-api-version";
const HEADER_INTERACTION_TYPE: &str = "x-interaction-type";
/// The intent and interaction type Copilot expects on its native Anthropic
/// endpoints, matching the `LiteLLM` Copilot messages transformation.
const MESSAGES_PROXY_INTENT: &str = "messages-proxy";
const INITIATOR_USER: &str = "user";
pub const INITIATOR_AGENT: &str = "agent";

/// Which request body shape a path carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyShape {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl BodyShape {
    fn from_path(path: &str) -> Option<BodyShape> {
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

/// Headers for a Copilot `/models` call: the flow's fixed set.
#[must_use]
pub fn models_headers(flow: CopilotFlow) -> Vec<(HeaderName, HeaderValue)> {
    match flow {
        // A gh token gets 400 "IDE token is malformed" when Copilot-Integration-Id or
        // X-GitHub-Api-Version is present, so this flow sends neither.
        CopilotFlow::Opencode => vec![
            (
                axum::http::header::USER_AGENT,
                HeaderValue::from_str(&format!("llm-hub/{VERSION}"))
                    .unwrap_or_else(|_| HeaderValue::from_static("llm-hub")),
            ),
            static_header(HEADER_OPENAI_INTENT, COPILOT_OPENAI_INTENT),
        ],
        CopilotFlow::Vscode => vec![
            static_header(HEADER_EDITOR_VERSION, COPILOT_EDITOR_VERSION),
            static_header(HEADER_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_PLUGIN_VERSION),
            (
                axum::http::header::USER_AGENT,
                HeaderValue::from_static(COPILOT_VSCODE_USER_AGENT),
            ),
            static_header(HEADER_INTEGRATION_ID, COPILOT_INTEGRATION_ID),
            static_header(HEADER_OPENAI_INTENT, COPILOT_OPENAI_INTENT),
            static_header(HEADER_API_VERSION, COPILOT_VSCODE_API_VERSION),
        ],
    }
}

/// Headers for a Copilot inference call on `path`: the flow's fixed set plus
/// `X-Initiator`, and `Copilot-Vision-Request` when the body holds an image.
/// The native Anthropic paths swap `Openai-Intent` for `messages-proxy` and
/// add `X-Interaction-Type`. A body that is not JSON, or a path with no known
/// shape, counts as a user turn without images.
#[must_use]
pub fn request_headers(
    flow: CopilotFlow,
    path: &str,
    body: &[u8],
) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = models_headers(flow);
    if is_messages_proxy_path(path) {
        for (name, value) in &mut headers {
            if name.as_str() == HEADER_OPENAI_INTENT {
                *value = HeaderValue::from_static(MESSAGES_PROXY_INTENT);
            }
        }
        headers.push(static_header(
            HEADER_INTERACTION_TYPE,
            MESSAGES_PROXY_INTENT,
        ));
    }
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let shape = BodyShape::from_path(path);
    let (is_agent, has_vision) = match (shape, parsed.as_ref()) {
        (Some(shape), Some(body)) => (is_agent_turn(shape, body), has_image(shape, body)),
        _ => (false, false),
    };
    let initiator = if is_agent {
        INITIATOR_AGENT
    } else {
        INITIATOR_USER
    };
    headers.push(static_header(HEADER_X_INITIATOR, initiator));
    if has_vision {
        headers.push(static_header(HEADER_VISION_REQUEST, "true"));
    }
    headers
}

/// Whether `path` is Copilot's native Anthropic `/messages` or
/// `/messages/count_tokens` endpoint.
fn is_messages_proxy_path(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.ends_with("/messages") || path.ends_with("/messages/count_tokens")
}

fn static_header(name: &'static str, value: &'static str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}

/// Whether the last turn is a tool-loop follow-up rather than a fresh user turn.
fn is_agent_turn(shape: BodyShape, body: &Value) -> bool {
    match shape {
        BodyShape::ChatCompletions => body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .is_some_and(|last| role(last) != Some("user")),
        BodyShape::Responses => match body.get("input") {
            Some(Value::Array(items)) => items.last().is_some_and(|last| !is_user_item(last)),
            _ => false,
        },
        BodyShape::AnthropicMessages => body
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .is_some_and(|last| role(last) != Some("user") || is_only_tool_results(last)),
    }
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// A Responses input item is a user turn when it is a `user` message; an item
/// without a `type` is a message.
fn is_user_item(item: &Value) -> bool {
    let is_message = item
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| kind == "message");
    is_message && role(item) == Some("user")
}

fn is_only_tool_results(message: &Value) -> bool {
    match message.get("content") {
        Some(Value::Array(parts)) => {
            !parts.is_empty()
                && parts
                    .iter()
                    .all(|part| part_type(part) == Some("tool_result"))
        }
        _ => false,
    }
}

fn part_type(part: &Value) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

fn has_image(shape: BodyShape, body: &Value) -> bool {
    let (key, image_type) = match shape {
        BodyShape::ChatCompletions => ("messages", "image_url"),
        BodyShape::Responses => ("input", "input_image"),
        BodyShape::AnthropicMessages => ("messages", "image"),
    };
    let Some(Value::Array(items)) = body.get(key) else {
        return false;
    };
    items.iter().any(|item| {
        // Responses items can be content parts themselves, not only messages.
        part_type(item) == Some(image_type) || content_has_image(item, image_type)
    })
}

/// Whether `message.content` holds an `image_type` part, looking inside
/// nested `content` arrays such as Anthropic `tool_result` parts.
fn content_has_image(message: &Value, image_type: &str) -> bool {
    let Some(Value::Array(parts)) = message.get("content") else {
        return false;
    };
    parts
        .iter()
        .any(|part| part_type(part) == Some(image_type) || content_has_image(part, image_type))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CHAT: &str = "/v1/chat/completions";
    const RESPONSES: &str = "/v1/responses";
    const MESSAGES: &str = "/v1/messages";

    fn header<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .and_then(|(_, value)| value.to_str().ok())
    }

    fn initiator(path: &str, body: &Value) -> String {
        let headers = request_headers(CopilotFlow::Opencode, path, body.to_string().as_bytes());
        header(&headers, HEADER_X_INITIATOR).unwrap().to_string()
    }

    fn vision(path: &str, body: &Value) -> bool {
        let headers = request_headers(CopilotFlow::Opencode, path, body.to_string().as_bytes());
        header(&headers, HEADER_VISION_REQUEST) == Some("true")
    }

    #[test]
    fn opencode_flow_omits_integration_id_and_api_version() {
        let headers = request_headers(CopilotFlow::Opencode, CHAT, b"{}");
        assert_eq!(header(&headers, HEADER_INTEGRATION_ID), None);
        assert_eq!(header(&headers, HEADER_API_VERSION), None);
        assert_eq!(header(&headers, HEADER_EDITOR_VERSION), None);
        assert_eq!(
            header(&headers, "user-agent"),
            Some(format!("llm-hub/{VERSION}").as_str())
        );
        assert_eq!(
            header(&headers, HEADER_OPENAI_INTENT),
            Some("conversation-edits")
        );
    }

    #[test]
    fn vscode_flow_carries_editor_and_integration_headers() {
        let headers = request_headers(CopilotFlow::Vscode, CHAT, b"{}");
        assert_eq!(header(&headers, HEADER_INTEGRATION_ID), Some("vscode-chat"));
        assert_eq!(header(&headers, HEADER_API_VERSION), Some("2025-04-01"));
        assert_eq!(
            header(&headers, HEADER_EDITOR_VERSION),
            Some("vscode/1.107.0")
        );
        assert_eq!(
            header(&headers, HEADER_EDITOR_PLUGIN_VERSION),
            Some("copilot-chat/0.35.0")
        );
        assert_eq!(
            header(&headers, "user-agent"),
            Some("GitHubCopilotChat/0.35.0")
        );
        assert_eq!(
            header(&headers, HEADER_OPENAI_INTENT),
            Some("conversation-edits")
        );
    }

    #[test]
    fn messages_paths_send_messages_proxy_intent() {
        for flow in [CopilotFlow::Opencode, CopilotFlow::Vscode] {
            for path in [MESSAGES, "/v1/messages/count_tokens"] {
                let headers = request_headers(flow, path, b"{}");
                assert_eq!(
                    header(&headers, HEADER_OPENAI_INTENT),
                    Some("messages-proxy")
                );
                assert_eq!(
                    header(&headers, HEADER_INTERACTION_TYPE),
                    Some("messages-proxy")
                );
                let intents = headers
                    .iter()
                    .filter(|(name, _)| name.as_str() == HEADER_OPENAI_INTENT)
                    .count();
                assert_eq!(intents, 1);
            }
        }
        let opencode = request_headers(CopilotFlow::Opencode, MESSAGES, b"{}");
        assert_eq!(header(&opencode, HEADER_API_VERSION), None);
    }

    #[test]
    fn other_paths_keep_generic_intent_without_interaction_type() {
        for path in [CHAT, RESPONSES] {
            let headers = request_headers(CopilotFlow::Opencode, path, b"{}");
            assert_eq!(
                header(&headers, HEADER_OPENAI_INTENT),
                Some("conversation-edits")
            );
            assert_eq!(header(&headers, HEADER_INTERACTION_TYPE), None);
        }
        let models = models_headers(CopilotFlow::Opencode);
        assert_eq!(header(&models, HEADER_INTERACTION_TYPE), None);
    }

    #[test]
    fn models_headers_have_no_per_request_values() {
        for flow in [CopilotFlow::Opencode, CopilotFlow::Vscode] {
            let headers = models_headers(flow);
            assert_eq!(header(&headers, HEADER_X_INITIATOR), None);
            assert_eq!(header(&headers, HEADER_VISION_REQUEST), None);
        }
        assert_eq!(
            header(
                &models_headers(CopilotFlow::Opencode),
                HEADER_INTEGRATION_ID
            ),
            None
        );
    }

    #[test]
    fn chat_user_turn_is_user() {
        let body = json!({"messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hi"}
        ]});
        assert_eq!(initiator(CHAT, &body), "user");
    }

    #[test]
    fn chat_tool_or_assistant_last_is_agent() {
        let tool = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": []},
            {"role": "tool", "tool_call_id": "1", "content": "ok"}
        ]});
        assert_eq!(initiator(CHAT, &tool), "agent");
        let assistant = json!({"messages": [{"role": "assistant", "content": "x"}]});
        assert_eq!(initiator(CHAT, &assistant), "agent");
    }

    #[test]
    fn responses_function_call_output_last_is_agent() {
        let body = json!({"input": [
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "1", "output": "ok"}
        ]});
        assert_eq!(initiator(RESPONSES, &body), "agent");
    }

    #[test]
    fn responses_user_message_or_string_input_is_user() {
        let body = json!({"input": [{"type": "message", "role": "user", "content": "hi"}]});
        assert_eq!(initiator(RESPONSES, &body), "user");
        assert_eq!(initiator(RESPONSES, &json!({"input": "hi"})), "user");
    }

    #[test]
    fn anthropic_tool_result_only_user_message_is_agent() {
        let body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "1", "name": "f", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "1", "content": "ok"}]}
        ]});
        assert_eq!(initiator(MESSAGES, &body), "agent");
    }

    #[test]
    fn anthropic_user_message_with_text_is_user() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "1", "content": "ok"},
            {"type": "text", "text": "now do this"}
        ]}]});
        assert_eq!(initiator(MESSAGES, &body), "user");
        let plain = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(initiator(MESSAGES, &plain), "user");
    }

    #[test]
    fn vision_is_detected_for_chat_image_url() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "what is this"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}}
        ]}]});
        assert!(vision(CHAT, &body));
        assert!(!vision(
            CHAT,
            &json!({"messages": [{"role": "user", "content": "hi"}]})
        ));
    }

    #[test]
    fn vision_is_detected_for_responses_input_image() {
        let body = json!({"input": [{"role": "user", "content": [
            {"type": "input_text", "text": "what is this"},
            {"type": "input_image", "image_url": "data:image/png;base64,AA"}
        ]}]});
        assert!(vision(RESPONSES, &body));
        assert!(!vision(RESPONSES, &json!({"input": "hi"})));
    }

    #[test]
    fn vision_is_detected_for_anthropic_image_including_inside_tool_result() {
        let direct = json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
        ]}]});
        assert!(vision(MESSAGES, &direct));
        let nested = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "1", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
            ]}
        ]}]});
        assert!(vision(MESSAGES, &nested));
        assert!(!vision(
            MESSAGES,
            &json!({"messages": [{"role": "user", "content": "hi"}]})
        ));
    }

    #[test]
    fn garbage_body_is_user_without_vision() {
        let headers = request_headers(CopilotFlow::Opencode, CHAT, b"not json{");
        assert_eq!(header(&headers, HEADER_X_INITIATOR), Some("user"));
        assert_eq!(header(&headers, HEADER_VISION_REQUEST), None);
        let headers = request_headers(CopilotFlow::Vscode, MESSAGES, b"[1, 2]");
        assert_eq!(header(&headers, HEADER_X_INITIATOR), Some("user"));
    }

    #[test]
    fn unknown_path_is_user_without_vision() {
        let body = json!({"messages": [{"role": "assistant", "content": "x"}]});
        assert_eq!(initiator("/v1/embeddings", &body), "user");
    }
}

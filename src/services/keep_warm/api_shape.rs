use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use axum::http::HeaderValue;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::schemas::prompt_shape::PromptShape;

#[cfg(test)]
const MESSAGES_PATH: &str = "/v1/messages";
/// The Responses API refuses `max_output_tokens` below 16.
const RESPONSES_MIN_OUTPUT_TOKENS: u64 = 16;

/// The API a kept request used. It decides how the ping is built and how a stream is told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiShape {
    /// Anthropic `/v1/messages`.
    Messages,
    /// `OpenAI` `/chat/completions`.
    ChatCompletions,
    /// `OpenAI` `/responses`.
    Responses,
}

/// Where a ping goes and what it carries besides the body.
#[derive(Debug, Clone)]
pub(crate) struct PingTarget {
    pub(crate) shape: ApiShape,
    /// The upstream path the request went to, query string included.
    pub(crate) path: String,
    /// The client's `anthropic-beta` header, which lets the upstream accept beta body fields.
    pub(crate) beta: Option<HeaderValue>,
}

impl PingTarget {
    #[cfg(test)]
    pub(crate) fn messages(beta: Option<HeaderValue>) -> PingTarget {
        PingTarget {
            shape: ApiShape::Messages,
            path: MESSAGES_PATH.to_owned(),
            beta,
        }
    }
}

impl ApiShape {
    /// The shape of a request sent to `path`, or `None` for a path whose prompt cache a ping
    /// cannot refresh (embeddings, audio, token counts).
    pub(crate) fn from_upstream_path(path: &str) -> Option<ApiShape> {
        let bare = path.split('?').next().unwrap_or(path).trim_end_matches('/');
        if bare.ends_with("/chat/completions") {
            Some(ApiShape::ChatCompletions)
        } else if bare.ends_with("/responses") {
            Some(ApiShape::Responses)
        } else if bare.ends_with("/messages") {
            Some(ApiShape::Messages)
        } else {
            None
        }
    }

    /// The accepted body as a ping: not streamed, and the answer capped as low as the API allows,
    /// so the ping pays for a cache read and next to no output.
    pub(crate) fn ping_body(self, body: &[u8]) -> Option<Bytes> {
        let mut request: Value = serde_json::from_slice(body).ok()?;
        let fields = request.as_object_mut()?;
        fields.insert("stream".to_owned(), Value::Bool(false));
        match self {
            ApiShape::Messages => {
                fields.insert("max_tokens".to_owned(), json!(1));
            }
            ApiShape::ChatCompletions => {
                fields.remove("stream_options");
                // Reasoning models take only `max_completion_tokens`; older servers only `max_tokens`.
                let newer = fields.contains_key("max_completion_tokens");
                if newer {
                    fields.insert("max_completion_tokens".to_owned(), json!(1));
                }
                if fields.contains_key("max_tokens") || !newer {
                    fields.insert("max_tokens".to_owned(), json!(1));
                }
            }
            ApiShape::Responses => {
                fields.remove("stream_options");
                fields.insert(
                    "max_output_tokens".to_owned(),
                    json!(RESPONSES_MIN_OUTPUT_TOKENS),
                );
            }
        }
        serde_json::to_vec(&request).ok().map(Bytes::from)
    }

    /// A hash of the system prompt and the tool names: requests with the same key come from the
    /// same conversation or subagent type. No prompt text is kept.
    pub(crate) fn stream_key(self, body: &[u8]) -> Option<String> {
        match self {
            ApiShape::Messages => PromptShape::from_body(body).and_then(|shape| shape.stream_key),
            ApiShape::ChatCompletions | ApiShape::Responses => {
                let request: Value = serde_json::from_slice(body).ok()?;
                request.as_object()?;
                Some(Self::openai_key(&request))
            }
        }
    }

    fn openai_key(request: &Value) -> String {
        let mut hasher = DefaultHasher::new();
        if let Some(instructions) = request.get("instructions").and_then(Value::as_str) {
            instructions.hash(&mut hasher);
        }
        let turns = request
            .get("messages")
            .or_else(|| request.get("input"))
            .and_then(Value::as_array);
        for turn in turns.into_iter().flatten() {
            let role = turn.get("role").and_then(Value::as_str);
            if matches!(role, Some("system" | "developer")) {
                Self::text_of(turn.get("content")).hash(&mut hasher);
            }
        }
        0u8.hash(&mut hasher);
        let mut tools: Vec<&str> = request
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| {
                tool.get("name")
                    .or_else(|| tool.pointer("/function/name"))
                    .and_then(Value::as_str)
            })
            .collect();
        tools.sort_unstable();
        tools.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// A message's text, whether `content` is a string or a list of parts.
    fn text_of(content: Option<&Value>) -> String {
        match content {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(shape: ApiShape, body: &Value) -> Value {
        let ping = shape.ping_body(body.to_string().as_bytes()).unwrap();
        serde_json::from_slice(&ping).unwrap()
    }

    #[test]
    fn the_shape_follows_the_upstream_path() {
        let shape = ApiShape::from_upstream_path;
        assert_eq!(shape("/v1/messages"), Some(ApiShape::Messages));
        assert_eq!(shape("/chat/completions"), Some(ApiShape::ChatCompletions));
        assert_eq!(
            shape("/openai/deployments/x/chat/completions?api-version=1"),
            Some(ApiShape::ChatCompletions)
        );
        assert_eq!(shape("/responses"), Some(ApiShape::Responses));
        assert_eq!(shape("/v1/messages/count_tokens"), None);
        assert_eq!(shape("/embeddings"), None);
        assert_eq!(shape("/audio/transcriptions"), None);
    }

    #[test]
    fn a_chat_ping_is_not_streamed_and_caps_the_answer() {
        let body = json!({
            "model": "m", "stream": true, "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "hi"}],
        });
        let sent = ping(ApiShape::ChatCompletions, &body);
        assert_eq!(sent["stream"], json!(false));
        assert_eq!(sent["max_tokens"], json!(1));
        assert!(sent.get("stream_options").is_none());
        assert_eq!(sent["messages"], body["messages"]);
    }

    #[test]
    fn a_chat_ping_caps_the_field_the_request_used() {
        let reasoning = json!({"model": "m", "max_completion_tokens": 4096, "messages": []});
        let sent = ping(ApiShape::ChatCompletions, &reasoning);
        assert_eq!(sent["max_completion_tokens"], json!(1));
        assert!(sent.get("max_tokens").is_none());

        let both =
            json!({"model": "m", "max_tokens": 9, "max_completion_tokens": 9, "messages": []});
        let sent = ping(ApiShape::ChatCompletions, &both);
        assert_eq!(
            (
                sent["max_tokens"].clone(),
                sent["max_completion_tokens"].clone()
            ),
            (json!(1), json!(1))
        );
    }

    #[test]
    fn a_responses_ping_keeps_the_sixteen_token_floor() {
        let body = json!({"model": "m", "stream": true, "input": "hi", "max_output_tokens": 5000});
        let sent = ping(ApiShape::Responses, &body);
        assert_eq!(sent["max_output_tokens"], json!(16));
        assert_eq!(sent["stream"], json!(false));
    }

    #[test]
    fn a_messages_ping_caps_max_tokens() {
        let sent = ping(
            ApiShape::Messages,
            &json!({"model": "m", "max_tokens": 900, "stream": true}),
        );
        assert_eq!(
            (sent["max_tokens"].clone(), sent["stream"].clone()),
            (json!(1), json!(false))
        );
    }

    #[test]
    fn an_openai_stream_key_follows_the_system_prompt_and_tool_names_only() {
        let key = |body: Value| {
            ApiShape::ChatCompletions
                .stream_key(body.to_string().as_bytes())
                .unwrap()
        };
        let main = json!({
            "messages": [{"role": "system", "content": "You are the main agent."}, {"role": "user", "content": "first"}],
            "tools": [{"type": "function", "function": {"name": "bash"}}, {"type": "function", "function": {"name": "read"}}],
        });
        let later = json!({
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "You are the main agent."}]},
                {"role": "user", "content": "later"}, {"role": "assistant", "content": "ok"},
            ],
            "tools": [{"type": "function", "function": {"name": "read", "description": "changed"}}, {"type": "function", "function": {"name": "bash"}}],
        });
        let subagent = json!({
            "messages": [{"role": "system", "content": "You are a planner."}],
            "tools": [{"type": "function", "function": {"name": "bash"}}, {"type": "function", "function": {"name": "read"}}],
        });
        assert_eq!(key(main.clone()), key(later));
        assert_ne!(key(main), key(subagent));
    }

    #[test]
    fn a_responses_key_reads_instructions_and_flat_tool_names() {
        let key = |body: Value| {
            ApiShape::Responses
                .stream_key(body.to_string().as_bytes())
                .unwrap()
        };
        let one = json!({"instructions": "Be brief.", "input": "a", "tools": [{"type": "function", "name": "bash"}]});
        let two = json!({"instructions": "Be brief.", "input": "b", "tools": [{"type": "function", "name": "bash"}]});
        let other = json!({"instructions": "Be long.", "input": "a", "tools": [{"type": "function", "name": "bash"}]});
        assert_eq!(key(one.clone()), key(two));
        assert_ne!(key(one), key(other));
    }
}

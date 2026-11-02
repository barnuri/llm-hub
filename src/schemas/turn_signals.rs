use serde_json::Value;

use crate::schemas::hub_selector::HubSelector;
use crate::services::copilot_auto::{AutoTier, ConversationKey, Turn, estimated_prompt_tokens};

/// What one request body says about its latest turn, read once from the parsed body.
/// Smart Auto routes on these signals and never reads the body again.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Smart Auto routing reads only the prompt estimate so far.
pub struct TurnSignals {
    /// Roughly one token per four request bytes.
    pub estimated_prompt_tokens: u64,
    /// The latest turn carries no new user text, such as a tool result, so the pin replays.
    pub tool_continuation: bool,
    /// The number of real user turns, tool-result turns excluded.
    pub user_turns: usize,
    /// The most recent real user text, cut to at most 8 KiB on a character boundary.
    pub last_user_text: Option<String>,
    /// The body declares at least one tool.
    pub has_tools: bool,
    /// The latest turn holds an image part.
    pub has_image: bool,
    /// The caller's session id, the strongest affinity input.
    pub session_id: Option<String>,
    /// The body's `prompt_cache_key`, the affinity input after the session id.
    pub prompt_cache_key: Option<String>,
    /// A hash of the system prompt and first user message; `None` for an unknown body shape.
    pub content_key: Option<ConversationKey>,
}

impl TurnSignals {
    const MAX_LAST_USER_TEXT_BYTES: usize = 8 * 1024;
    const TOOLS_FIELD: &str = "tools";
    const PROMPT_CACHE_KEY_FIELD: &str = "prompt_cache_key";

    /// Reads the signals of a body whose shape `path` names. `prompt_bytes` is the raw request
    /// size. An unknown path yields only the estimate, the tool flag and the affinity inputs.
    #[must_use]
    pub fn from_body(
        path: &str,
        body: &Value,
        prompt_bytes: usize,
        session_id: Option<String>,
    ) -> TurnSignals {
        // The content key ignores the tier, so a tier switch keeps the same conversation.
        let turn = Turn::parse(HubSelector::NAMESPACE, AutoTier::default(), path, body);
        let user_turns = turn.as_ref().map_or(0, |turn| turn.user_turns);
        TurnSignals {
            estimated_prompt_tokens: estimated_prompt_tokens(prompt_bytes),
            tool_continuation: turn
                .as_ref()
                .is_some_and(|turn| turn.prompt.is_none() && user_turns > 0),
            user_turns,
            has_image: turn.as_ref().is_some_and(|turn| turn.has_image),
            content_key: turn.as_ref().map(|turn| turn.key),
            last_user_text: turn.and_then(|turn| turn.last_user_text).map(Self::bounded),
            has_tools: body
                .get(Self::TOOLS_FIELD)
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty()),
            session_id,
            prompt_cache_key: body
                .get(Self::PROMPT_CACHE_KEY_FIELD)
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty())
                .map(str::to_string),
        }
    }

    fn bounded(mut text: String) -> String {
        if text.len() > Self::MAX_LAST_USER_TEXT_BYTES {
            text.truncate(text.floor_char_boundary(Self::MAX_LAST_USER_TEXT_BYTES));
        }
        text
    }
}

#[cfg(test)]
impl TurnSignals {
    /// The signals of one plain user turn with no text, tools, image or affinity inputs.
    /// Tests set the fields they need with struct update syntax.
    pub(crate) fn for_test() -> TurnSignals {
        TurnSignals {
            estimated_prompt_tokens: 0,
            tool_continuation: false,
            user_turns: 1,
            last_user_text: None,
            has_tools: false,
            has_image: false,
            session_id: None,
            prompt_cache_key: None,
            content_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CHAT_PATH: &str = "/v1/chat/completions";
    const RESPONSES_PATH: &str = "/v1/responses";
    const MESSAGES_PATH: &str = "/v1/messages";
    const BODY_BYTES: usize = 4000;

    fn signals(path: &str, body: &Value) -> TurnSignals {
        TurnSignals::from_body(path, body, BODY_BYTES, None)
    }

    #[test]
    fn chat_first_turn_reads_text_tools_and_cache_key() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "plan the change"}
            ],
            "tools": [{"type": "function", "function": {"name": "read"}}],
            "prompt_cache_key": "pck-1"
        });

        let signals = TurnSignals::from_body(CHAT_PATH, &body, BODY_BYTES, Some("s-1".into()));

        assert_eq!(signals.estimated_prompt_tokens, 1000);
        assert!(!signals.tool_continuation);
        assert_eq!(signals.user_turns, 1);
        assert_eq!(signals.last_user_text.as_deref(), Some("plan the change"));
        assert!(signals.has_tools);
        assert!(!signals.has_image);
        assert_eq!(signals.session_id.as_deref(), Some("s-1"));
        assert_eq!(signals.prompt_cache_key.as_deref(), Some("pck-1"));
        assert!(signals.content_key.is_some());
    }

    #[test]
    fn chat_tool_result_is_a_continuation_and_keeps_the_last_user_text() {
        let body = json!({"messages": [
            {"role": "user", "content": "fix it"},
            {"role": "assistant", "tool_calls": [{"id": "c1"}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"}
        ]});

        let signals = signals(CHAT_PATH, &body);

        assert!(signals.tool_continuation);
        assert_eq!(signals.user_turns, 1);
        assert_eq!(signals.last_user_text.as_deref(), Some("fix it"));
        assert!(!signals.has_tools);
        assert_eq!(signals.prompt_cache_key, None);
    }

    #[test]
    fn content_key_is_stable_across_turns() {
        let first = json!({"messages": [{"role": "user", "content": "hi"}]});
        let later = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "more"}
        ]});

        let later_signals = signals(CHAT_PATH, &later);

        assert_eq!(
            signals(CHAT_PATH, &first).content_key,
            later_signals.content_key
        );
        assert_eq!(later_signals.user_turns, 2);
    }

    #[test]
    fn responses_function_call_output_is_a_continuation_and_string_input_is_a_turn() {
        let continuation = json!({"input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]},
            {"type": "function_call", "call_id": "c1", "name": "read"},
            {"type": "function_call_output", "call_id": "c1", "output": "done"}
        ]});
        let fresh = json!({"input": "describe the image", "tools": []});

        let continued = signals(RESPONSES_PATH, &continuation);
        let first = signals(RESPONSES_PATH, &fresh);

        assert!(continued.tool_continuation);
        assert_eq!(continued.last_user_text.as_deref(), Some("go"));
        assert!(!first.tool_continuation);
        assert_eq!(first.user_turns, 1);
        assert!(!first.has_tools);
    }

    #[test]
    fn messages_tool_result_is_a_continuation_and_image_is_detected() {
        let continuation = json!({
            "system": "sys",
            "messages": [
                {"role": "user", "content": "look"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1"}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]}
            ],
            "tools": [{"name": "read"}]
        });
        let image = json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {}},
            {"type": "text", "text": "what is this"}
        ]}]});

        let continued = signals(MESSAGES_PATH, &continuation);
        let pictured = signals(MESSAGES_PATH, &image);

        assert!(continued.tool_continuation);
        assert!(continued.has_tools);
        assert!(pictured.has_image);
        assert!(!pictured.tool_continuation);
    }

    #[test]
    fn last_user_text_is_cut_to_8_kib_on_a_char_boundary() {
        let long = "é".repeat(TurnSignals::MAX_LAST_USER_TEXT_BYTES);
        let body = json!({"messages": [{"role": "user", "content": long}]});

        let text = signals(CHAT_PATH, &body).last_user_text.unwrap();

        assert_eq!(text.len(), TurnSignals::MAX_LAST_USER_TEXT_BYTES);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn unknown_path_keeps_only_the_estimate_and_affinity_inputs() {
        let body = json!({"input": "embed me", "prompt_cache_key": "k"});

        let signals = signals("/v1/embeddings", &body);

        assert_eq!(signals.estimated_prompt_tokens, 1000);
        assert_eq!(signals.user_turns, 0);
        assert_eq!(signals.content_key, None);
        assert_eq!(signals.prompt_cache_key.as_deref(), Some("k"));
    }
}

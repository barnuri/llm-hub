//! Joins a chat completion that Copilot split across choices back into one assistant message.

use serde_json::{Map, Value, json};

/// Copilot's non-streaming Claude reply puts the text in one choice and each tool call in its own
/// choice. Code that reads `choices[0]` then loses the tool calls, so the buffered review path merges them first.
#[derive(Debug)]
pub struct SplitChoices;

impl SplitChoices {
    const TOOL_CALLS: &'static str = "tool_calls";

    /// Merges the choices of `completion` into choice 0 when they are one split reply. Leaves a
    /// single choice, or real `n > 1` alternatives (more than one choice with text and no tool call), unchanged.
    pub fn merge(completion: &mut Value) {
        let Some(choices) = completion.get_mut("choices").and_then(Value::as_array_mut) else {
            return;
        };
        if choices.len() < 2 || !Self::is_split(choices) {
            return;
        }
        let mut message = choices[0]
            .get("message")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: Vec<Value> = Vec::new();
        let mut finish = choices[0]
            .get("finish_reason")
            .cloned()
            .unwrap_or(Value::Null);
        for choice in choices.iter() {
            let part = choice.get("message");
            Self::push_text(part, "content", &mut content);
            Self::push_text(part, "reasoning_content", &mut reasoning);
            if let Some(list) = part
                .and_then(|m| m.get(Self::TOOL_CALLS))
                .and_then(Value::as_array)
            {
                calls.extend(list.iter().cloned());
            }
            if choice.get("finish_reason").and_then(Value::as_str) == Some(Self::TOOL_CALLS) {
                finish = json!(Self::TOOL_CALLS);
            }
        }
        Self::set_text(&mut message, "content", content);
        Self::set_text(&mut message, "reasoning_content", reasoning);
        if !calls.is_empty() {
            message.insert(Self::TOOL_CALLS.to_owned(), Value::Array(calls));
        }
        let mut merged = choices[0].as_object().cloned().unwrap_or_default();
        merged.insert("index".to_owned(), json!(0));
        merged.insert("message".to_owned(), Value::Object(message));
        merged.insert("finish_reason".to_owned(), finish);
        *choices = vec![Value::Object(merged)];
    }

    /// True when at most one choice carries text without tool calls, so the choices are parts of one reply.
    fn is_split(choices: &[Value]) -> bool {
        let text_only = choices
            .iter()
            .filter(|choice| {
                let message = choice.get("message");
                let has_calls = message
                    .and_then(|m| m.get(Self::TOOL_CALLS))
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
                !has_calls
            })
            .count();
        text_only <= 1
    }

    fn push_text(message: Option<&Value>, key: &str, out: &mut String) {
        if let Some(text) = message.and_then(|m| m.get(key)).and_then(Value::as_str) {
            out.push_str(text);
        }
    }

    fn set_text(message: &mut Map<String, Value>, key: &str, text: String) {
        if !text.is_empty() {
            message.insert(key.to_owned(), Value::String(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str) -> Value {
        json!({ "id": id, "type": "function", "function": { "name": name, "arguments": "{}" } })
    }

    /// The shape Copilot returns for Claude: text in choice 0, one tool call per later choice.
    fn copilot_split() -> Value {
        json!({
            "id": "chatcmpl-1",
            "choices": [
                { "index": 0, "finish_reason": "tool_calls",
                  "message": { "role": "assistant", "content": "Checking both." } },
                { "index": 1, "finish_reason": "tool_calls",
                  "message": { "role": "assistant", "tool_calls": [call("a", "get_weather")] } },
                { "index": 2, "finish_reason": "tool_calls",
                  "message": { "role": "assistant", "tool_calls": [call("b", "get_time")] } },
            ],
            "usage": { "completion_tokens": 9 },
        })
    }

    #[test]
    fn merges_text_and_every_tool_call_into_choice_zero() {
        let mut reply = copilot_split();
        SplitChoices::merge(&mut reply);
        let choices = reply["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0]["index"], 0);
        assert_eq!(choices[0]["finish_reason"], "tool_calls");
        assert_eq!(choices[0]["message"]["content"], "Checking both.");
        assert_eq!(choices[0]["message"]["role"], "assistant");
        let names: Vec<&str> = choices[0]["message"]["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["get_weather", "get_time"]);
        assert_eq!(reply["usage"]["completion_tokens"], 9);
    }

    #[test]
    fn takes_tool_calls_finish_from_a_later_choice() {
        let mut reply = copilot_split();
        reply["choices"][0]["finish_reason"] = json!("stop");
        SplitChoices::merge(&mut reply);
        assert_eq!(reply["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn joins_reasoning_and_keeps_a_reply_with_no_text() {
        let mut reply = json!({ "choices": [
            { "index": 0, "finish_reason": "tool_calls",
              "message": { "reasoning_content": "think", "tool_calls": [call("a", "x")] } },
            { "index": 1, "finish_reason": "tool_calls",
              "message": { "tool_calls": [call("b", "y")] } },
        ] });
        SplitChoices::merge(&mut reply);
        let message = &reply["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "think");
        assert!(message.get("content").is_none());
        assert_eq!(message["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn leaves_a_single_choice_unchanged() {
        let mut reply = json!({ "choices": [
            { "index": 0, "finish_reason": "stop", "message": { "content": "hi" } },
        ] });
        let before = reply.clone();
        SplitChoices::merge(&mut reply);
        assert_eq!(reply, before);
    }

    #[test]
    fn leaves_real_alternatives_unchanged() {
        let mut reply = json!({ "choices": [
            { "index": 0, "finish_reason": "stop", "message": { "content": "one" } },
            { "index": 1, "finish_reason": "stop", "message": { "content": "two" } },
        ] });
        let before = reply.clone();
        SplitChoices::merge(&mut reply);
        assert_eq!(reply, before);
    }

    #[test]
    fn ignores_a_reply_without_choices() {
        let mut reply = json!({ "error": "x" });
        SplitChoices::merge(&mut reply);
        assert_eq!(reply, json!({ "error": "x" }));
    }
}

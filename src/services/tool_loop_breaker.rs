//! Deterministic breaker for no-progress tool loops.
//!
//! Some local models, once they make a tool call that fails, repeat the exact
//! same call forever: each repeat gets the same result, and at temperature 0
//! the longer history only reinforces the pattern. A prompt hint does not stop
//! that reliably, so when the request history ends in [`REPEAT_THRESHOLD`]
//! identical steps (same calls, same arguments, same results) the request is
//! rewritten: the last result carries a note naming the call, and the looping
//! tool is withheld from `tools` for this one request, so the model cannot
//! emit it again. The next request from the client carries the full tool list
//! again.

use serde_json::{Map, Value, json};

/// Identical trailing steps that count as a loop. Two identical polls of a
/// still-running command are plausible; three identical results are not
/// progress.
const REPEAT_THRESHOLD: usize = 3;
const RESULT_PREVIEW_CHARS: usize = 200;

/// One assistant tool-call message plus the tool results answering it.
struct ToolStep {
    /// `[name, arguments, result]` per call, in call order. Arguments are
    /// parsed so key order and whitespace do not hide a repeat.
    signature: Vec<Value>,
    /// Index of the assistant message that opens the step.
    start: usize,
}

/// Rewrites the request when its history ends in a tool loop. Returns the
/// looping tool names, or `None` when the body was left untouched.
pub fn break_repeated_tool_calls(obj: &mut Map<String, Value>) -> Option<Vec<String>> {
    let messages = obj.get("messages")?.as_array()?;
    let last = step_ending_at(messages, messages.len())?;
    let repeats = trailing_repeat_count(messages, &last);
    tracing::debug!(repeats, "trailing identical tool steps");
    if repeats < REPEAT_THRESHOLD {
        return None;
    }

    let names = looping_tool_names(&last.signature);
    let note = loop_note(&last.signature);
    let messages = obj.get_mut("messages")?.as_array_mut()?;
    append_note(messages.last_mut()?, &note);
    withhold_tools(obj, &names);
    tracing::warn!(tools = ?names, "tool loop detected; withholding the looping tools for one request");
    Some(names)
}

fn trailing_repeat_count(messages: &[Value], last: &ToolStep) -> usize {
    let mut count = 1;
    let mut end = last.start;
    while count < REPEAT_THRESHOLD {
        let Some(step) = step_ending_at(messages, end) else {
            break;
        };
        if step.signature != last.signature {
            break;
        }
        count += 1;
        end = step.start;
    }
    count
}

/// Parses the step whose last tool result sits just before `end`.
fn step_ending_at(messages: &[Value], end: usize) -> Option<ToolStep> {
    let results_start = messages[..end]
        .iter()
        .rposition(|message| role(message) != Some("tool"))
        .map_or(0, |index| index + 1);
    if results_start == end || results_start == 0 {
        return None;
    }

    let assistant_index = results_start - 1;
    let assistant = &messages[assistant_index];
    if role(assistant) != Some("assistant") {
        return None;
    }
    let calls = assistant.get("tool_calls")?.as_array()?;
    if calls.is_empty() {
        return None;
    }

    let results = &messages[results_start..end];
    let signature = calls
        .iter()
        .map(|call| call_signature(call, results))
        .collect();
    Some(ToolStep {
        signature,
        start: assistant_index,
    })
}

fn call_signature(call: &Value, results: &[Value]) -> Value {
    let function = call.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .cloned()
        .unwrap_or(Value::Null);
    let arguments = function
        .and_then(|f| f.get("arguments"))
        .map_or(Value::Null, parse_arguments);
    let id = call.get("id").and_then(Value::as_str);
    let result = results
        .iter()
        .find(|message| message.get("tool_call_id").and_then(Value::as_str) == id)
        .and_then(|message| message.get("content"))
        .cloned()
        .unwrap_or(Value::Null);
    json!([name, arguments, result])
}

fn parse_arguments(raw: &Value) -> Value {
    raw.as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| raw.clone())
}

fn looping_tool_names(signature: &[Value]) -> Vec<String> {
    let mut names: Vec<String> = signature
        .iter()
        .filter_map(|call| call[0].as_str().map(String::from))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn loop_note(signature: &[Value]) -> String {
    let calls: Vec<String> = signature
        .iter()
        .map(|call| {
            format!(
                "`{}` with arguments {} (result: {})",
                call[0].as_str().unwrap_or("?"),
                call[1],
                preview(&call[2])
            )
        })
        .collect();
    format!(
        "\n\n[llm-hub tool-loop breaker] The same call has now run {REPEAT_THRESHOLD} times in a row \
         and returned the same result every time: {}. Repeating it cannot make progress, so that \
         tool is unavailable for this turn. Do not call it again with these arguments; continue \
         the task using the output you already have or a different tool.",
        calls.join("; ")
    )
}

fn preview(result: &Value) -> String {
    let text = match result {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let mut cut: String = text.chars().take(RESULT_PREVIEW_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    format!("{cut:?}")
}

fn append_note(message: &mut Value, note: &str) {
    match message.get_mut("content") {
        Some(Value::String(text)) => text.push_str(note),
        Some(Value::Array(parts)) => parts.push(json!({"type": "text", "text": note.trim_start()})),
        _ => message["content"] = Value::String(note.trim_start().to_string()),
    }
}

/// Drops the looping tools from `tools`, unless that would leave no tools or
/// `tool_choice` forces one of them — either would turn a loop into a 400.
fn withhold_tools(obj: &mut Map<String, Value>, names: &[String]) {
    if obj
        .get("tool_choice")
        .and_then(|choice| choice.pointer("/function/name"))
        .and_then(Value::as_str)
        .is_some_and(|forced| names.iter().any(|name| name == forced))
    {
        return;
    }
    let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    let is_looping = |tool: &Value| {
        tool.pointer("/function/name")
            .and_then(Value::as_str)
            .is_some_and(|name| names.iter().any(|looping| looping == name))
    };
    if tools.iter().all(is_looping) {
        return;
    }
    tools.retain(|tool| !is_looping(tool));
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILED: &str =
        "No agent found with agent_id: 0. The agent may have been cleared or never existed.";

    fn tool(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "parameters": {}}})
    }

    fn step(id: &str, name: &str, arguments: &str, result: &str) -> [Value; 2] {
        [
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": id, "type": "function", "function": {"name": name, "arguments": arguments}}
            ]}),
            json!({"role": "tool", "tool_call_id": id, "content": result}),
        ]
    }

    fn read_agent_step(id: &str) -> [Value; 2] {
        step(id, "read_agent", r#"{"agent_id":"0","wait":true}"#, FAILED)
    }

    fn request(steps: Vec<[Value; 2]>) -> Map<String, Value> {
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "fix the tests"}),
        ];
        messages.extend(steps.into_iter().flatten());
        json!({
            "model": "llama_swap/qwen3.6-35b",
            "messages": messages,
            "tools": [tool("bash"), tool("view"), tool("read_agent")],
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn tool_names(obj: &Map<String, Value>) -> Vec<&str> {
        obj["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect()
    }

    fn last_content(obj: &Map<String, Value>) -> &Value {
        &obj["messages"].as_array().unwrap().last().unwrap()["content"]
    }

    #[test]
    fn three_identical_failing_calls_withhold_the_tool_and_name_the_call() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
        ]);

        let names = break_repeated_tool_calls(&mut obj);

        assert_eq!(names, Some(vec!["read_agent".to_string()]));
        assert_eq!(tool_names(&obj), vec!["bash", "view"]);
        let content = last_content(&obj).as_str().unwrap();
        assert!(content.starts_with(FAILED));
        assert!(content.contains("[llm-hub tool-loop breaker]"));
        assert!(content.contains(r#"`read_agent` with arguments {"agent_id":"0","wait":true}"#));
    }

    #[test]
    fn fewer_repeats_than_the_threshold_leave_the_body_untouched() {
        let mut obj = request(vec![read_agent_step("a"), read_agent_step("b")]);
        let before = obj.clone();

        assert_eq!(break_repeated_tool_calls(&mut obj), None);
        assert_eq!(obj, before);
    }

    #[test]
    fn a_loop_that_is_not_at_the_end_of_history_is_ignored() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
            step("d", "view", r#"{"path":"calc.py"}"#, "def average(xs): ..."),
        ]);
        let before = obj.clone();

        assert_eq!(break_repeated_tool_calls(&mut obj), None);
        assert_eq!(obj, before);
    }

    #[test]
    fn same_call_with_changing_results_is_progress_not_a_loop() {
        let poll = |id: &str, output: &str| step(id, "read_bash", r#"{"shellId":"3"}"#, output);
        let mut obj = request(vec![
            poll("a", "line 1"),
            poll("b", "line 2"),
            poll("c", "line 3"),
        ]);
        let before = obj.clone();

        assert_eq!(break_repeated_tool_calls(&mut obj), None);
        assert_eq!(obj, before);
    }

    #[test]
    fn argument_key_order_and_whitespace_do_not_hide_a_repeat() {
        let mut obj = request(vec![
            read_agent_step("a"),
            step(
                "b",
                "read_agent",
                r#"{ "wait": true, "agent_id": "0" }"#,
                FAILED,
            ),
            read_agent_step("c"),
        ]);

        assert!(break_repeated_tool_calls(&mut obj).is_some());
    }

    #[test]
    fn different_arguments_are_not_a_repeat() {
        let mut obj = request(vec![
            read_agent_step("a"),
            step("b", "read_agent", r#"{"agent_id":"1","wait":true}"#, FAILED),
            read_agent_step("c"),
        ]);

        assert_eq!(break_repeated_tool_calls(&mut obj), None);
    }

    #[test]
    fn the_only_tool_is_kept_so_the_request_stays_valid() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
        ]);
        obj["tools"] = json!([tool("read_agent")]);

        assert!(break_repeated_tool_calls(&mut obj).is_some());
        assert_eq!(tool_names(&obj), vec!["read_agent"]);
        assert!(
            last_content(&obj)
                .as_str()
                .unwrap()
                .contains("tool-loop breaker")
        );
    }

    #[test]
    fn a_forced_tool_choice_keeps_the_tool() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
        ]);
        obj.insert(
            "tool_choice".into(),
            json!({"type": "function", "function": {"name": "read_agent"}}),
        );

        assert!(break_repeated_tool_calls(&mut obj).is_some());
        assert_eq!(tool_names(&obj), vec!["bash", "view", "read_agent"]);
    }

    #[test]
    fn array_content_gets_the_note_as_a_new_text_part() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
        ]);
        let messages = obj["messages"].as_array_mut().unwrap();
        for message in messages.iter_mut().filter(|m| m["role"] == "tool") {
            message["content"] = json!([{"type": "text", "text": FAILED}]);
        }

        assert!(break_repeated_tool_calls(&mut obj).is_some());
        let parts = last_content(&obj).as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("[llm-hub tool-loop breaker]")
        );
    }

    #[test]
    fn history_ending_in_a_user_or_assistant_message_is_untouched() {
        let mut obj = request(vec![
            read_agent_step("a"),
            read_agent_step("b"),
            read_agent_step("c"),
        ]);
        obj["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "user", "content": "keep going"}));
        let before = obj.clone();

        assert_eq!(break_repeated_tool_calls(&mut obj), None);
        assert_eq!(obj, before);
    }
}

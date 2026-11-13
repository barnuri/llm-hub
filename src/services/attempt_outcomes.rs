//! Verified-outcome signals for completed Smart Auto attempts: the test-output
//! classifier, and the last completed attempt of each worker.

use dashmap::DashMap;
use serde_json::Value;

use crate::schemas::attempt_outcome::OutcomeCode;

/// The last completed solver attempt of each worker, so a later signal can find the attempt it judges.
#[derive(Default)]
pub struct AttemptTracker {
    completed: DashMap<String, String>,
}

impl AttemptTracker {
    pub fn note_completed(&self, worker_key: &str, attempt_key: &str) {
        self.completed
            .insert(worker_key.to_string(), attempt_key.to_string());
    }

    pub fn last_completed(&self, worker_key: &str) -> Option<String> {
        self.completed
            .get(worker_key)
            .map(|entry| entry.value().clone())
    }
}

/// Classifies test-runner output: cargo, pytest, jest/vitest and go test. A fail marker
/// wins over a pass marker. `None` when the text has no runner summary.
pub fn classify_test_output(text: &str) -> Option<OutcomeCode> {
    let mut saw_pass = false;
    for line in text.lines() {
        let line = line.trim();
        if line_fails(line) {
            return Some(OutcomeCode::TestsFailed);
        }
        saw_pass |= line_passes(line);
    }
    saw_pass.then_some(OutcomeCode::TestsPassed)
}

fn line_fails(line: &str) -> bool {
    line.starts_with("test result: FAILED")
        || line == "FAIL"
        || line.starts_with("FAIL\t")
        || line.starts_with("FAIL ")
        || line.starts_with("--- FAIL")
        || count_before(line, "failed").is_some_and(|n| n > 0)
        || count_before(line, "errors").is_some_and(|n| n > 0)
        || count_before(line, "error").is_some_and(|n| n > 0) && is_summary(line)
}

fn line_passes(line: &str) -> bool {
    line.starts_with("test result: ok.")
        || line == "PASS"
        || line.starts_with("ok  ")
        || line.starts_with("ok\t")
        || count_before(line, "passed").is_some_and(|n| n > 0)
}

/// A pytest or jest summary line, where `N error` counts collection errors rather than log text.
fn is_summary(line: &str) -> bool {
    line.starts_with('=') || line.starts_with("Tests:") || line.starts_with("Test Files")
}

/// The number written just before `word`, as in `3 passed` or `1 failed,`.
fn count_before(line: &str, word: &str) -> Option<u64> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    tokens.windows(2).find_map(|pair| {
        let next = pair[1].trim_end_matches([',', ';', '.', ')']);
        if next == word {
            pair[0].trim_start_matches('(').parse().ok()
        } else {
            None
        }
    })
}

/// The tool-result text of the request's last message or trailing tool messages, joined.
/// Handles `OpenAI` chat (`role: tool`), Anthropic messages (`tool_result` blocks) and
/// Responses input (`function_call_output`). `None` when the request ends with no tool result.
pub fn last_tool_result_text(body: &Value) -> Option<String> {
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)?;
    let mut texts = Vec::new();
    for item in items.iter().rev() {
        collect_tool_text(item, &mut texts);
        // Parallel tool calls arrive as several trailing tool items; anything else ends the turn's results.
        let is_tool_item = item.get("role").and_then(Value::as_str) == Some("tool")
            || item.get("type").and_then(Value::as_str) == Some("function_call_output");
        if !is_tool_item {
            break;
        }
    }
    if texts.is_empty() {
        return None;
    }
    texts.reverse();
    Some(texts.join("\n"))
}

fn collect_tool_text(item: &Value, texts: &mut Vec<String>) {
    if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
        if let Some(output) = item.get("output").and_then(Value::as_str) {
            texts.push(output.to_string());
        }
        return;
    }
    let content = item.get("content");
    if item.get("role").and_then(Value::as_str) == Some("tool") {
        if let Some(text) = content.and_then(text_of) {
            texts.push(text);
        }
        return;
    }
    let Some(blocks) = content.and_then(Value::as_array) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("tool_result")
            && let Some(text) = block.get("content").and_then(text_of)
        {
            texts.push(text);
        }
    }
}

/// A string, or the joined `text` of an array of content blocks.
fn text_of(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_cargo_test_results() {
        assert_eq!(
            classify_test_output(
                "running 7 tests\ntest a ... ok\n\ntest result: ok. 7 passed; 0 failed; 0 ignored"
            ),
            Some(OutcomeCode::TestsPassed)
        );
        assert_eq!(
            classify_test_output("test b ... FAILED\ntest result: FAILED. 2 passed; 1 failed"),
            Some(OutcomeCode::TestsFailed)
        );
    }

    #[test]
    fn classifies_pytest() {
        assert_eq!(
            classify_test_output("================ 3 passed in 0.21s ================"),
            Some(OutcomeCode::TestsPassed)
        );
        assert_eq!(
            classify_test_output("========= 1 failed, 3 passed in 0.40s ========="),
            Some(OutcomeCode::TestsFailed)
        );
        assert_eq!(
            classify_test_output("=========== 2 errors in 0.10s ==========="),
            Some(OutcomeCode::TestsFailed)
        );
    }

    #[test]
    fn classifies_jest_and_vitest() {
        assert_eq!(
            classify_test_output("Tests:       4 passed, 1 failed, 5 total"),
            Some(OutcomeCode::TestsFailed)
        );
        assert_eq!(
            classify_test_output(" Test Files  3 passed (3)\n      Tests  12 passed (12)"),
            Some(OutcomeCode::TestsPassed)
        );
    }

    #[test]
    fn classifies_go_test() {
        assert_eq!(
            classify_test_output("ok  \texample.com/pkg\t0.123s"),
            Some(OutcomeCode::TestsPassed)
        );
        assert_eq!(
            classify_test_output("--- FAIL: TestX (0.00s)\nFAIL\nFAIL\texample.com/pkg\t0.2s"),
            Some(OutcomeCode::TestsFailed)
        );
    }

    #[test]
    fn ignores_text_without_a_runner_summary() {
        assert_eq!(classify_test_output("build complete"), None);
        assert_eq!(classify_test_output("the request failed to parse"), None);
        assert_eq!(classify_test_output("0 failed"), None);
    }

    #[test]
    fn reads_trailing_openai_tool_messages() {
        let body = json!({"messages": [
            {"role": "user", "content": "run tests"},
            {"role": "assistant", "content": null},
            {"role": "tool", "content": "test result: ok. 1 passed; 0 failed"},
            {"role": "tool", "content": "lint clean"}
        ]});
        assert_eq!(
            last_tool_result_text(&body).as_deref(),
            Some("test result: ok. 1 passed; 0 failed\nlint clean")
        );
    }

    #[test]
    fn reads_anthropic_tool_result_blocks() {
        let body = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": [{"type": "text", "text": "3 passed"}]}
            ]}
        ]});
        assert_eq!(last_tool_result_text(&body).as_deref(), Some("3 passed"));
    }

    #[test]
    fn reads_responses_function_call_output() {
        let body = json!({"input": [
            {"type": "function_call", "call_id": "c", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c", "output": "ok  \tpkg\t0.1s"}
        ]});
        assert_eq!(
            last_tool_result_text(&body).as_deref(),
            Some("ok  \tpkg\t0.1s")
        );
    }

    #[test]
    fn a_plain_user_message_has_no_tool_result() {
        let body = json!({"messages": [
            {"role": "tool", "content": "3 passed"},
            {"role": "user", "content": "thanks, now 1 failed elsewhere"}
        ]});
        assert_eq!(last_tool_result_text(&body), None);
    }

    #[test]
    fn tracker_remembers_last_completed_attempt() {
        let tracker = AttemptTracker::default();
        assert_eq!(tracker.last_completed("worker-a"), None);
        tracker.note_completed("worker-a", "attempt-1");
        tracker.note_completed("worker-a", "attempt-2");
        assert_eq!(tracker.last_completed("worker-a"), Some("attempt-2".into()));
    }
}

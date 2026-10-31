use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;

/// Body of `POST {api}/auto`, Copilot's model routing decision for one prompt.
#[derive(Clone, Deserialize)]
pub struct CopilotAutoResponse {
    /// Token the chat request sends as `Copilot-Session-Token`. Never log or format it.
    pub session_token: String,
    /// Unix seconds at which `session_token` expires.
    pub expires_at: u64,
    /// The chosen model, shaped like a `/models` entry; only `id` is guaranteed.
    pub selected_model: Value,
    pub selection_reason: Option<String>,
    /// Task-type scores such as `code_gen` and `reasoning`, not per-model scores.
    #[serde(default)]
    pub hydra_scores: HashMap<String, f64>,
    /// The server's multi-turn schedule: skip window, backoff and escalation threshold.
    pub multi_turn: Option<Value>,
    /// Model id to the Auto discount as a fraction, e.g. `0.1` for 10% off.
    #[serde(default)]
    pub discounted_costs: HashMap<String, f64>,
}

// Hand-written so the session token can never reach a log line through `{:?}`.
impl fmt::Debug for CopilotAutoResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotAutoResponse")
            .field("session_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("selected_model", &self.selected_model)
            .field("selection_reason", &self.selection_reason)
            .field("hydra_scores", &self.hydra_scores)
            .field("multi_turn", &self.multi_turn)
            .field("discounted_costs", &self.discounted_costs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "auto-session-secret";

    fn live_body() -> String {
        format!(
            r#"{{"session_token":"{SECRET}","expires_at":1793556562,
                "selected_model":{{"id":"gpt-6.1-sol","supported_endpoints":["/responses"],
                    "billing":{{"token_prices":{{"default":{{"input_price":200,"output_price":1000}}}}}}}},
                "multi_turn":{{"enabled":true,"sigma":{{"code_gen":0.23}},"escalate_threshold":2,
                    "initial_skip":2,"backoff_coefficient":2,"max_skip":32,"anchor_skip":0,"context_window":5}},
                "multi_turn_mode":"model_cards",
                "hydra_scores":{{"code_gen":0.5773,"reasoning":0.3342}},
                "discounted_costs":{{"gpt-6.1-sol":0.1}},
                "selection_reason":"Auto selected GPT-6.1 Sol since the task has medium complexity."}}"#
        )
    }

    #[test]
    fn deserialises_live_body() {
        let response: CopilotAutoResponse = serde_json::from_str(&live_body()).unwrap();
        assert_eq!(response.session_token, SECRET);
        assert_eq!(response.expires_at, 1_793_556_562);
        assert_eq!(response.selected_model["id"], "gpt-6.1-sol");
        assert_eq!(response.hydra_scores.get("code_gen"), Some(&0.5773));
        assert_eq!(response.discounted_costs.get("gpt-6.1-sol"), Some(&0.1));
        assert_eq!(response.multi_turn.unwrap()["initial_skip"], 2);
        assert!(
            response
                .selection_reason
                .unwrap()
                .starts_with("Auto selected")
        );
    }

    #[test]
    fn missing_optional_fields_default() {
        let response: CopilotAutoResponse = serde_json::from_str(
            r#"{"session_token":"t","expires_at":1,"selected_model":{"id":"m"}}"#,
        )
        .unwrap();
        assert!(response.hydra_scores.is_empty());
        assert!(response.discounted_costs.is_empty());
        assert_eq!(response.multi_turn, None);
        assert_eq!(response.selection_reason, None);
    }

    #[test]
    fn missing_session_token_is_an_error() {
        let parsed = serde_json::from_str::<CopilotAutoResponse>(
            r#"{"expires_at":1,"selected_model":{"id":"m"}}"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn debug_redacts_session_token() {
        let response: CopilotAutoResponse = serde_json::from_str(&live_body()).unwrap();
        let debug = format!("{response:?}");
        assert!(!debug.contains(SECRET));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("gpt-6.1-sol"));
    }
}

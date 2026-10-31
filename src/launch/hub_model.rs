use serde_json::Value;

/// One `/v1/models` entry, with the window the hub reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubModel {
    id: String,
    context_window: Option<u64>,
}

impl HubModel {
    /// Output budget cap, matching the reserve the benchmark harnesses use.
    const MAX_OUTPUT_TOKENS: u64 = 32_768;

    #[cfg(test)]
    #[must_use]
    pub fn new(id: &str, context_window: Option<u64>) -> Self {
        Self {
            id: id.to_string(),
            context_window,
        }
    }

    /// Every entry of a `/v1/models` payload that has a string `id`.
    #[must_use]
    pub fn list_from(payload: &Value) -> Vec<Self> {
        payload
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Self::from_entry)
            .collect()
    }

    /// The catalog entry for `id`.
    #[must_use]
    pub fn find<'a>(catalog: &'a [Self], id: &str) -> Option<&'a Self> {
        catalog.iter().find(|model| model.id == id)
    }

    fn from_entry(entry: &Value) -> Option<Self> {
        Some(Self {
            id: entry.get("id")?.as_str()?.to_string(),
            context_window: entry.get("context_window").and_then(Value::as_u64),
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// A quarter of the window, capped at 32768 tokens.
    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.context_window
            .map(|window| (window / 4).min(Self::MAX_OUTPUT_TOKENS))
    }

    /// The window minus the output budget.
    #[must_use]
    pub fn max_prompt_tokens(&self) -> Option<u64> {
        Some(self.context_window? - self.max_output_tokens()?)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn list_reads_ids_and_windows_and_skips_entries_without_id() {
        let payload = json!({"data": [
            {"id": "copilot/claude-opus-5-5", "context_window": 1_000_000},
            {"id": "llama_swap/glm-4.7-flash"},
            {"object": "model"},
        ]});

        let models = HubModel::list_from(&payload);

        assert_eq!(
            models,
            [
                HubModel::new("copilot/claude-opus-5-5", Some(1_000_000)),
                HubModel::new("llama_swap/glm-4.7-flash", None),
            ]
        );
    }

    #[test]
    fn payload_without_data_is_an_empty_catalog() {
        assert!(HubModel::list_from(&json!({"error": "nope"})).is_empty());
    }

    #[test]
    fn output_budget_is_a_quarter_of_a_small_window() {
        let model = HubModel::new("m", Some(65_536));

        assert_eq!(model.max_output_tokens(), Some(16_384));
        assert_eq!(model.max_prompt_tokens(), Some(49_152));
    }

    #[test]
    fn output_budget_is_capped_for_a_large_window() {
        let model = HubModel::new("m", Some(1_000_000));

        assert_eq!(model.max_output_tokens(), Some(32_768));
        assert_eq!(model.max_prompt_tokens(), Some(967_232));
    }

    #[test]
    fn unknown_window_has_no_budgets() {
        let model = HubModel::new("m", None);

        assert_eq!(model.max_output_tokens(), None);
        assert_eq!(model.max_prompt_tokens(), None);
    }

    #[test]
    fn find_matches_the_exact_id() {
        let catalog = [HubModel::new("a/b", Some(1)), HubModel::new("a/c", None)];

        assert_eq!(HubModel::find(&catalog, "a/c"), Some(&catalog[1]));
        assert_eq!(HubModel::find(&catalog, "a"), None);
    }
}

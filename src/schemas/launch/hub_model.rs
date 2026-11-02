use serde_json::Value;

/// One `/v1/models` entry, with the window and output limit the hub reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubModel {
    id: String,
    context_window: Option<u64>,
    advertised_max_output_tokens: Option<u64>,
}

impl HubModel {
    /// Output budget cap, matching the reserve the benchmark harnesses use.
    const MAX_OUTPUT_TOKENS: u64 = 32_768;

    /// A catalog entry with the given window.
    #[cfg(test)]
    #[must_use]
    pub fn new(id: &str, context_window: Option<u64>) -> Self {
        Self {
            id: id.to_string(),
            context_window,
            advertised_max_output_tokens: None,
        }
    }

    /// A catalog entry whose hub entry advertises `max_output_tokens`.
    #[cfg(test)]
    #[must_use]
    pub fn with_max_output_tokens(mut self, tokens: u64) -> Self {
        self.advertised_max_output_tokens = Some(tokens);
        self
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
            advertised_max_output_tokens: entry.get("max_output_tokens").and_then(Value::as_u64),
        })
    }

    /// The hub model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The context window the hub reports, in tokens.
    #[must_use]
    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// The hub's advertised output limit, else a quarter of the window capped at 32768 tokens.
    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.advertised_max_output_tokens
            .or_else(|| self.derived_output_budget())
    }

    /// The window minus the derived output budget. The advertised window is already the prompt
    /// limit, so a larger advertised output limit does not shrink it.
    #[must_use]
    pub fn max_prompt_tokens(&self) -> Option<u64> {
        self.context_window?
            .checked_sub(self.derived_output_budget()?)
            .filter(|tokens| *tokens > 0)
    }

    fn derived_output_budget(&self) -> Option<u64> {
        self.context_window
            .map(|window| (window / 4).min(Self::MAX_OUTPUT_TOKENS))
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
    fn advertised_output_limit_replaces_the_derived_budget() {
        let payload = json!({"data": [
            {"id": "copilot/claude-opus-5-5[1m]", "context_window": 1_000_000,
             "max_output_tokens": 128_000},
        ]});

        let model = &HubModel::list_from(&payload)[0];

        assert_eq!(
            model,
            &HubModel::new("copilot/claude-opus-5-5[1m]", Some(1_000_000))
                .with_max_output_tokens(128_000)
        );
        assert_eq!(model.max_output_tokens(), Some(128_000));
        assert_eq!(model.max_prompt_tokens(), Some(967_232));
    }

    #[test]
    fn output_limit_filling_the_window_keeps_the_derived_prompt_budget() {
        let model = HubModel::new("m", Some(64_000)).with_max_output_tokens(64_000);

        assert_eq!(model.max_output_tokens(), Some(64_000));
        assert_eq!(model.max_prompt_tokens(), Some(48_000));
    }

    #[test]
    fn find_matches_the_exact_id() {
        let catalog = [HubModel::new("a/b", Some(1)), HubModel::new("a/c", None)];

        assert_eq!(HubModel::find(&catalog, "a/c"), Some(&catalog[1]));
        assert_eq!(HubModel::find(&catalog, "a"), None);
    }
}

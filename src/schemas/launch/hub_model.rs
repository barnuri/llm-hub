use serde_json::Value;

use super::hub_model_price::HubModelPrice;

/// One `/v1/models` entry: the window, output limit, picker label and price the hub reports for it.
#[derive(Debug, Clone, PartialEq)]
pub struct HubModel {
    id: String,
    context_window: Option<u64>,
    advertised_max_output_tokens: Option<u64>,
    display_name: Option<String>,
    recommended: bool,
    vision: bool,
    price: Option<HubModelPrice>,
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
            display_name: None,
            recommended: false,
            vision: false,
            price: None,
        }
    }

    /// A catalog entry with a picker label, marked recommended or not.
    #[cfg(test)]
    #[must_use]
    pub fn with_display_name(mut self, name: &str, recommended: bool) -> Self {
        self.display_name = Some(name.to_string());
        self.recommended = recommended;
        self
    }

    /// A catalog entry that takes images.
    #[cfg(test)]
    #[must_use]
    pub fn with_vision(mut self) -> Self {
        self.vision = true;
        self
    }

    /// A catalog entry with a listed price.
    #[cfg(test)]
    #[must_use]
    pub fn with_price(mut self, price: HubModelPrice) -> Self {
        self.price = Some(price);
        self
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
            display_name: entry
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string),
            recommended: entry.get("recommended").and_then(Value::as_bool) == Some(true),
            vision: entry
                .pointer("/capabilities/supports/vision")
                .and_then(Value::as_bool)
                == Some(true),
            price: HubModelPrice::from_entry(entry),
        })
    }

    /// The picker label the hub lists, with " (recommended)" on the hub's recommended entry.
    #[must_use]
    pub fn picker_label(&self) -> Option<String> {
        let name = self.display_name.as_deref()?;
        Some(if self.recommended {
            format!("{name} (recommended)")
        } else {
            name.to_string()
        })
    }

    /// Whether the model takes image and PDF attachments.
    #[must_use]
    pub fn takes_attachments(&self) -> bool {
        self.vision
    }

    /// The price the hub lists, in USD per one million tokens.
    #[must_use]
    pub fn price(&self) -> Option<&HubModelPrice> {
        self.price.as_ref()
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
    fn list_reads_the_picker_label_vision_and_price() {
        let payload = json!({"data": [
            {"id": "hub/smart-auto-balance", "display_name": "Smart Auto (Balance)",
             "recommended": true},
            {"id": "copilot/claude-opus-5-5", "display_name": "claude-opus-5-5 · copilot · $4/$20",
             "capabilities": {"supports": {"vision": true}},
             "llm_hub_price": {"input_usd_per_1m": 4.0, "output_usd_per_1m": 20.0}},
        ]});

        let models = HubModel::list_from(&payload);

        assert_eq!(
            models[0].picker_label().as_deref(),
            Some("Smart Auto (Balance) (recommended)")
        );
        assert!(!models[0].takes_attachments());
        assert_eq!(
            models[1].picker_label().as_deref(),
            Some("claude-opus-5-5 · copilot · $4/$20")
        );
        assert!(models[1].takes_attachments());
        assert_eq!(models[1].price(), Some(&HubModelPrice::new(4.0, 20.0)));
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

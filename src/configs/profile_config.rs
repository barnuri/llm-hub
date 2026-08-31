#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    /// Optional UI label; routing / model ids always use `name`.
    pub display_name: Option<String>,
    pub base_url: String,
    pub api_key: String,
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub enabled: bool,
    /// Static model list used only when the upstream exposes no /v1/models.
    pub static_models: Vec<String>,
    /// Default USD per 1M tokens for this profile (overridden by `model_prices`).
    pub pricing: crate::configs::TokenRates,
    /// Bare model id → rates for this profile.
    pub model_prices: std::collections::HashMap<String, crate::configs::TokenRates>,
}

impl ProfileConfig {
    /// Label shown in the UI — `display_name` when set, otherwise `name`.
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }
}

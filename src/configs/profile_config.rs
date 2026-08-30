#[derive(Debug, Clone, PartialEq)]
pub struct ProfileConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub extra_headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub enabled: bool,
    /// Static model list used only when the upstream exposes no /v1/models.
    pub static_models: Vec<String>,
}

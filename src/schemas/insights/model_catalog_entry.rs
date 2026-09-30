/// What the upstream model list declares about a model: its context window
/// and, for llama-swap rows, the use cases and summary from the model registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalogEntry {
    pub context_window: Option<u64>,
    pub use_cases: Vec<String>,
    pub summary: Option<String>,
}

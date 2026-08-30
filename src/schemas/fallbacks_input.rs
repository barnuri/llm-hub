use serde::Deserialize;

/// Admin API request body for setting the default fallback chain.
#[derive(Debug, Deserialize)]
pub struct FallbacksInput {
    pub fallbacks: Vec<String>,
}

use serde::Deserialize;
use utoipa::ToSchema;

/// Admin API request body for setting the default fallback chain.
#[derive(Debug, Deserialize, ToSchema)]
pub struct FallbacksInput {
    pub fallbacks: Vec<String>,
}

use serde::Deserialize;
use utoipa::ToSchema;

/// Admin API request body for creating a hub api key.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ApiKeyInput {
    pub name: String,
}

use serde::Deserialize;

/// Admin API request body for creating a hub api key.
#[derive(Debug, Deserialize)]
pub struct ApiKeyInput {
    pub name: String,
}

use std::collections::HashMap;

use serde::Deserialize;

/// Admin API request body for creating or updating a profile.
#[derive(Debug, Deserialize)]
pub struct ProfileInput {
    pub name: String,
    pub display_name: Option<String>,
    pub base_url: String,
    /// Omitted or empty on update => keep the existing key (write-only field).
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
    pub enabled: Option<bool>,
    pub models: Option<Vec<String>>,
}

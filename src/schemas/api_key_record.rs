use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct ApiKeyRecord {
    pub name: String,
    /// SHA-256 hex of the key — the key itself is never stored.
    pub key_hash: String,
    pub masked: String,
    pub enabled: bool,
    pub created_ms: u64,
}

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct KeepWarmInput {
    pub enabled: bool,
    pub idle_cap_minutes: u64,
}

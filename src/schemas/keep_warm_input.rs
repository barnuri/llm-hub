use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct KeepWarmInput {
    pub enabled: bool,
    pub idle_cap_minutes: u64,
}

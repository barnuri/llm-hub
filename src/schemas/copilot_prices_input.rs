use serde::Deserialize;
use utoipa::ToSchema;

/// Admin API request body for the Copilot price refresh interval.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CopilotPricesInput {
    /// Seconds between fetches; `0` turns the fetch off.
    pub refresh_secs: u64,
}

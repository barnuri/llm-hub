use std::fmt;

use serde::Deserialize;

use crate::schemas::copilot_endpoints::CopilotEndpoints;

/// Body of `GET copilot_internal/v2/token`, the VS Code flow's token exchange.
#[derive(Clone, Deserialize)]
pub struct CopilotTokenResponse {
    /// Short-lived Copilot bearer token. Never log or format it.
    pub token: String,
    /// Unix seconds at which `token` expires.
    pub expires_at: u64,
    /// Seconds until GitHub suggests refreshing `token`.
    pub refresh_in: Option<u64>,
    pub endpoints: Option<CopilotEndpoints>,
}

// Hand-written so the token can never reach a log line through `{:?}`.
impl fmt::Debug for CopilotTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotTokenResponse")
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("refresh_in", &self.refresh_in)
            .field("endpoints", &self.endpoints)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "tid=abc123;exp=1761930000;sku=copilot_for_business_seat;8kp=1:deadbeef";

    fn full_body() -> String {
        format!(
            r#"{{"token":"{SECRET}","expires_at":1761930000,"refresh_in":1500,
                "endpoints":{{"api":"https://api.business.githubcopilot.com",
                "origin-tracker":"https://origin-tracker.business.githubcopilot.com"}},
                "sku":"copilot_for_business_seat","chat_enabled":true}}"#
        )
    }

    #[test]
    fn deserialises_full_body() {
        let response: CopilotTokenResponse = serde_json::from_str(&full_body()).unwrap();
        assert_eq!(response.token, SECRET);
        assert_eq!(response.expires_at, 1_761_930_000);
        assert_eq!(response.refresh_in, Some(1500));
        assert_eq!(
            response.endpoints.and_then(|e| e.api).as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[test]
    fn missing_optional_fields_are_none() {
        let response: CopilotTokenResponse =
            serde_json::from_str(r#"{"token":"t","expires_at":1}"#).unwrap();
        assert_eq!(response.refresh_in, None);
        assert_eq!(response.endpoints, None);
    }

    #[test]
    fn missing_token_is_an_error() {
        assert!(serde_json::from_str::<CopilotTokenResponse>(r#"{"expires_at":1}"#).is_err());
    }

    #[test]
    fn debug_redacts_token() {
        let response: CopilotTokenResponse = serde_json::from_str(&full_body()).unwrap();
        let debug = format!("{response:?}");
        assert!(!debug.contains(SECRET));
        assert!(!debug.contains("abc123"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("1761930000"));
    }
}

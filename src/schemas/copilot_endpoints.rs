use serde::Deserialize;

/// The `endpoints` object GitHub returns from `copilot_internal/*`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CopilotEndpoints {
    /// Copilot API base for this account, e.g. `https://api.business.githubcopilot.com`.
    pub api: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_api_url() {
        let endpoints: CopilotEndpoints = serde_json::from_str(
            r#"{"api":"https://api.business.githubcopilot.com","proxy":"https://proxy.business.githubcopilot.com"}"#,
        )
        .unwrap();
        assert_eq!(
            endpoints.api.as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[test]
    fn missing_api_is_none() {
        let endpoints: CopilotEndpoints = serde_json::from_str("{}").unwrap();
        assert_eq!(endpoints.api, None);
    }
}

use serde::Deserialize;

use crate::schemas::copilot_endpoints::CopilotEndpoints;

/// Body of `GET copilot_internal/user`, the Flow B entitlement and base-URL lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CopilotUserResponse {
    pub chat_enabled: Option<bool>,
    pub can_signup_for_limited: Option<bool>,
    pub endpoints: Option<CopilotEndpoints>,
}

impl CopilotUserResponse {
    /// Why this account cannot chat, or `None` when it can.
    ///
    /// Only an explicit `chat_enabled: false` denies access; a missing field does not.
    /// The wording matches opencode's `copilotEntitlementError`.
    pub fn entitlement_error(&self) -> Option<&'static str> {
        if self.chat_enabled != Some(false) {
            return None;
        }
        if self.can_signup_for_limited == Some(true) {
            return Some(
                "This GitHub account is not signed up for GitHub Copilot. Sign up for Copilot Free at https://github.com/features/copilot/plans and connect again.",
            );
        }
        Some(
            "This GitHub account does not have GitHub Copilot access. It needs an active Copilot subscription or a seat assigned by an organization.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> CopilotUserResponse {
        serde_json::from_str(body).unwrap()
    }

    #[test]
    fn deserialises_business_body() {
        let user = parse(
            r#"{"login":"octocat","access_type_sku":"copilot_for_business_seat",
                "copilot_plan":"business","chat_enabled":true,"can_signup_for_limited":false,
                "endpoints":{"api":"https://api.business.githubcopilot.com",
                "proxy":"https://proxy.business.githubcopilot.com"}}"#,
        );
        assert_eq!(user.chat_enabled, Some(true));
        assert_eq!(user.can_signup_for_limited, Some(false));
        assert_eq!(
            user.endpoints.and_then(|e| e.api).as_deref(),
            Some("https://api.business.githubcopilot.com")
        );
    }

    #[test]
    fn missing_fields_are_none() {
        let user = parse(r#"{"login":"octocat"}"#);
        assert_eq!(user, CopilotUserResponse::default());
    }

    #[test]
    fn enabled_account_has_no_entitlement_error() {
        assert_eq!(parse(r#"{"chat_enabled":true}"#).entitlement_error(), None);
    }

    #[test]
    fn unknown_chat_state_has_no_entitlement_error() {
        assert_eq!(parse("{}").entitlement_error(), None);
    }

    #[test]
    fn signup_eligible_account_gets_signup_message() {
        let user = parse(r#"{"chat_enabled":false,"can_signup_for_limited":true}"#);
        let message = user.entitlement_error().unwrap();
        assert!(message.starts_with("This GitHub account is not signed up for GitHub Copilot."));
        assert!(message.contains("https://github.com/features/copilot/plans"));
    }

    #[test]
    fn ineligible_account_gets_no_access_message() {
        for body in [
            r#"{"chat_enabled":false,"can_signup_for_limited":false}"#,
            r#"{"chat_enabled":false}"#,
        ] {
            let message = parse(body).entitlement_error().unwrap();
            assert!(
                message.starts_with("This GitHub account does not have GitHub Copilot access.")
            );
        }
    }
}

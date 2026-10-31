use serde::Deserialize;

/// One entry of `quota_snapshots` in `copilot_internal/user`, such as `premium_interactions`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct CopilotQuotaSnapshot {
    pub entitlement: Option<f64>,
    pub remaining: Option<f64>,
    pub quota_remaining: Option<f64>,
    pub percent_remaining: Option<f64>,
    /// Authoritative for "used": it is not always `entitlement - remaining`.
    pub credits_used: Option<f64>,
    pub overage_permitted: Option<bool>,
    /// True when the amounts are AI credits (100 = $1) rather than premium requests.
    pub token_based_billing: Option<bool>,
    pub unlimited: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_token_billed_snapshot() {
        let snapshot: CopilotQuotaSnapshot = serde_json::from_str(
            r#"{"credits_used":12626,"entitlement":33000,"remaining":20489,"quota_remaining":20489.9,
                "percent_remaining":62.0,"overage_permitted":true,"token_based_billing":true,
                "unlimited":false,"quota_id":"premium_interactions"}"#,
        )
        .unwrap();
        assert_eq!(
            snapshot,
            CopilotQuotaSnapshot {
                entitlement: Some(33000.0),
                remaining: Some(20489.0),
                quota_remaining: Some(20489.9),
                percent_remaining: Some(62.0),
                credits_used: Some(12626.0),
                overage_permitted: Some(true),
                token_based_billing: Some(true),
                unlimited: Some(false),
            }
        );
    }

    #[test]
    fn missing_fields_are_none() {
        let snapshot: CopilotQuotaSnapshot = serde_json::from_str("{}").unwrap();
        assert_eq!(snapshot, CopilotQuotaSnapshot::default());
    }
}

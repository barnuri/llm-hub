//! A Bifrost profile's quota from its virtual-key quota endpoint, normalised to the shared
//! quota shape. The virtual key authenticates itself through `x-bf-vk`, so no admin token is needed.

use std::time::Duration;

use serde_json::Value;

use crate::services::profile_usage::{ProfileUsage, ProfileUsageQuota, QuotaUnit};

const QUOTA_PATH: &str = "/api/governance/virtual-keys/quota";
const OPENAI_SUFFIX: &str = "/v1";
const KEY_HEADER: &str = "x-bf-vk";
const SOURCE: &str = "bifrost/virtual-keys/quota";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Gateway root of an OpenAI-style base URL: the management API lives beside `/v1`, not under it.
fn quota_url(base_url: &str) -> String {
    let root = base_url.trim_end_matches('/');
    let root = root.strip_suffix(OPENAI_SUFFIX).unwrap_or(root);
    format!("{root}{QUOTA_PATH}")
}

/// Fetches the virtual key's budgets and rate limits. A failure is logged and returns `None`.
pub(crate) async fn fetch(
    client: &reqwest::Client,
    profile: &str,
    base_url: &str,
    api_key: &str,
    now_ms: u64,
) -> Option<ProfileUsage> {
    let url = quota_url(base_url);
    let response = client
        .get(&url)
        .header(KEY_HEADER, api_key)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .inspect_err(|e| tracing::warn!("profile {profile}: the Bifrost quota request failed: {e}"))
        .ok()?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!("profile {profile}: the Bifrost quota endpoint {url} returned {status}");
        return None;
    }
    let body: Value = response
        .json()
        .await
        .inspect_err(|e| tracing::warn!("profile {profile}: the Bifrost quota body is not JSON: {e}"))
        .ok()?;
    let usage = normalise(&body, now_ms);
    if usage.is_none() {
        tracing::warn!("profile {profile}: the Bifrost quota reports no budgets or rate limits");
    }
    usage
}

/// Budgets and rate limits of the virtual key and of each provider config, sorted by id.
/// `None` when the key has no limit at all.
fn normalise(body: &Value, fetched_at_ms: u64) -> Option<ProfileUsage> {
    let key = body.get("virtual_key").unwrap_or(body);
    let mut quotas = Vec::new();
    collect(key, "", &mut quotas);
    for config in key
        .get("provider_configs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let provider = config.get("provider").and_then(Value::as_str).unwrap_or("provider");
        collect(config, &format!("{provider}:"), &mut quotas);
    }
    if quotas.is_empty() {
        return None;
    }
    quotas.sort_by(|a, b| a.id.cmp(&b.id));
    Some(ProfileUsage::new(SOURCE, fetched_at_ms, quotas))
}

/// The `budgets` array and the `rate_limit` object of `owner`.
fn collect(owner: &Value, prefix: &str, quotas: &mut Vec<ProfileUsageQuota>) {
    for budget in owner.get("budgets").and_then(Value::as_array).into_iter().flatten() {
        let window = text(budget, "reset_duration").unwrap_or("total");
        let limit = number(budget, "max_limit").unwrap_or(0.0) + number(budget, "override_amount").unwrap_or(0.0);
        let used = number(budget, "current_usage").unwrap_or(0.0);
        quotas.extend(ProfileUsageQuota::measured(
            format!("{prefix}budget:{window}"),
            QuotaUnit::Usd,
            used,
            limit,
            None,
        ));
    }
    let Some(limits) = owner.get("rate_limit").filter(|v| v.is_object()) else {
        return;
    };
    for (unit, name, field) in [
        (QuotaUnit::Requests, "requests", "request"),
        (QuotaUnit::Tokens, "tokens", "token"),
    ] {
        let window = text(limits, &format!("{field}_reset_duration")).unwrap_or("total");
        let limit = number(limits, &format!("{field}_max_limit")).unwrap_or(0.0);
        let used = number(limits, &format!("{field}_current_usage")).unwrap_or(0.0);
        quotas.extend(ProfileUsageQuota::measured(
            format!("{prefix}{name}:{window}"),
            unit,
            used,
            limit,
            None,
        ));
    }
}

fn number(object: &Value, field: &str) -> Option<f64> {
    object.get(field).and_then(Value::as_f64)
}

fn text<'a>(object: &'a Value, field: &str) -> Option<&'a str> {
    object.get(field).and_then(Value::as_str).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NOW_MS: u64 = 1_793_476_800_000;

    fn quota_body() -> Value {
        json!({
            "id": "vk_1",
            "budgets": [{
                "id": "b1", "max_limit": 100.0, "current_usage": 25.0,
                "reset_duration": "1M", "last_reset": "2026-11-01T00:00:00Z",
            }],
            "rate_limit": {
                "request_max_limit": 1000, "request_current_usage": 250,
                "request_reset_duration": "1h",
                "token_max_limit": 0, "token_current_usage": 0,
            },
            "provider_configs": [{
                "provider": "openai",
                "budgets": [{"max_limit": 10.0, "current_usage": 10.0,
                             "reset_duration": "1d", "override_amount": 5.0}],
            }],
        })
    }

    fn ids(usage: &ProfileUsage) -> Vec<&str> {
        usage.quotas.iter().map(|q| q.id.as_str()).collect()
    }

    #[test]
    fn quota_url_sits_beside_v1() {
        assert_eq!(
            quota_url("https://gw.example.com/v1"),
            "https://gw.example.com/api/governance/virtual-keys/quota"
        );
        assert_eq!(
            quota_url("http://gw:8080/"),
            "http://gw:8080/api/governance/virtual-keys/quota"
        );
    }

    #[test]
    fn budgets_and_rate_limits_become_quotas() {
        let usage = normalise(&quota_body(), NOW_MS).unwrap();

        assert_eq!(
            ids(&usage),
            ["budget:1M", "openai:budget:1d", "requests:1h"]
        );
        let budget = &usage.quotas[0];
        assert_eq!(budget.unit, QuotaUnit::Usd);
        assert_eq!(budget.used_usd, Some(25.0));
        assert_eq!(budget.limit_usd, Some(100.0));
        assert!((budget.remaining_percentage - 75.0).abs() < 1e-9);
        let requests = &usage.quotas[2];
        assert_eq!(requests.unit, QuotaUnit::Requests);
        assert!((requests.remaining_percentage - 75.0).abs() < 1e-9);
    }

    #[test]
    fn budget_override_raises_the_limit() {
        let usage = normalise(&quota_body(), NOW_MS).unwrap();

        let provider = &usage.quotas[1];
        assert_eq!(provider.entitlement, 15.0);
        assert!((provider.remaining_percentage - 100.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn wrapped_virtual_key_is_accepted() {
        let usage = normalise(&json!({"virtual_key": quota_body()}), NOW_MS).unwrap();

        assert_eq!(usage.quotas.len(), 3);
    }

    #[test]
    fn key_without_limits_has_no_usage() {
        assert!(normalise(&json!({"id": "vk_1", "budgets": []}), NOW_MS).is_none());
    }

    #[tokio::test]
    async fn fetch_sends_the_virtual_key_header_to_the_gateway_root() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(QUOTA_PATH))
            .and(header(KEY_HEADER, "sk-bf-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(quota_body()))
            .expect(1)
            .mount(&server)
            .await;
        let base_url = format!("{}/v1", server.uri());

        let usage = fetch(&reqwest::Client::new(), "llmgw", &base_url, "sk-bf-secret", NOW_MS)
            .await
            .unwrap();

        assert_eq!(usage.source, SOURCE);
        assert_eq!(usage.updated_at, NOW_MS);
    }

    #[tokio::test]
    async fn rejected_key_has_no_usage() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(QUOTA_PATH))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let usage = fetch(&reqwest::Client::new(), "llmgw", &server.uri(), "sk-bf-bad", NOW_MS).await;

        assert!(usage.is_none());
    }
}

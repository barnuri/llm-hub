use std::collections::HashSet;

use axum::http::HeaderMap;

use crate::consts::{HEADER_FALLBACKS, HEADER_FALLBACKS_ALIAS, HEADER_RETRY_ON, HEADER_TIMEOUT_MS};
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;

/// Statuses that describe the request itself — identical on every upstream,
/// so retrying only multiplies latency and cost. Never retried even via 5xx rule.
const NEVER_RETRY: [u16; 5] = [400, 401, 403, 404, 422];
const DEFAULT_RETRY: [u16; 2] = [408, 429];

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    statuses: HashSet<u16>,
    include_5xx: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            statuses: DEFAULT_RETRY.into_iter().collect(),
            include_5xx: true,
        }
    }
}

impl RetryPolicy {
    /// `X-LLM-Hub-Retry-On: 429,503` replaces the default set entirely —
    /// an explicit override wins, including over `NEVER_RETRY`.
    pub fn from_headers(headers: &HeaderMap) -> RetryPolicy {
        let Some(raw) = header_str(headers, HEADER_RETRY_ON) else {
            return RetryPolicy::default();
        };
        let statuses: HashSet<u16> = raw
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if statuses.is_empty() {
            return RetryPolicy::default();
        }
        RetryPolicy {
            statuses,
            include_5xx: false,
        }
    }

    pub fn should_retry_status(&self, status: u16) -> bool {
        if self.statuses.contains(&status) {
            return true;
        }
        if !self.include_5xx {
            return false;
        }
        (500..=599).contains(&status) && !NEVER_RETRY.contains(&status)
    }
}

/// Ordered fallback chain from headers, falling back to the configured
/// default chain. Header (either name) wins over env default.
pub fn fallback_chain(headers: &HeaderMap, default_chain: &[String]) -> Vec<String> {
    let raw = header_str(headers, HEADER_FALLBACKS)
        .or_else(|| header_str(headers, HEADER_FALLBACKS_ALIAS));
    let Some(raw) = raw else {
        return default_chain.to_vec();
    };
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Keeps only fallbacks in the Smart Auto `eligible` pool, in chain order. Deny wins: a
/// selector, unknown or filtered model is dropped. `None` (not a `hub/*` route) keeps the chain.
#[must_use]
pub fn retain_eligible(chain: Vec<String>, eligible: Option<&[&ModelDescriptor]>) -> Vec<String> {
    let Some(eligible) = eligible else {
        return chain;
    };
    chain
        .into_iter()
        .filter(|entry| {
            let entry = strip_1m_suffix(entry);
            eligible
                .iter()
                .any(|descriptor| strip_1m_suffix(&descriptor.qualified_id) == entry)
        })
        .collect()
}

/// Validates and normalizes (trims) a default fallback chain before
/// persisting. Entries are stored comma-joined in the env file, so commas
/// inside an entry are rejected; profiles are deliberately not checked for
/// existence (a chain may reference a temporarily disabled profile).
///
/// # Errors
/// Fails when an entry is empty after trimming or contains a comma.
pub fn validate_chain(chain: &[String]) -> Result<Vec<String>, String> {
    let mut normalized = Vec::with_capacity(chain.len());
    for entry in chain {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err("fallback entries must not be empty".to_string());
        }
        if trimmed.contains(',') {
            return Err(format!("fallback entry may not contain ',': {trimmed}"));
        }
        normalized.push(trimmed.to_string());
    }
    Ok(normalized)
}

pub fn per_attempt_timeout_ms(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, HEADER_TIMEOUT_MS)?.trim().parse().ok()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(std::string::ToString::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::schemas::evidence_source::EvidenceSource;
    use crate::schemas::limit_evidence::LimitEvidence;
    use crate::schemas::model_descriptor_snapshot::ModelDescriptorSnapshot;
    use crate::schemas::smart_auto_policy::SmartAutoPolicy;
    use crate::services::smart_auto_pool::SmartAutoPool;
    use axum::http::HeaderValue;

    #[test]
    fn default_policy_retries_correct_statuses() {
        let policy = RetryPolicy::default();
        for status in [408, 429, 500, 502, 503, 599] {
            assert!(policy.should_retry_status(status), "{status} should retry");
        }
        for status in [200, 201, 400, 401, 403, 404, 422, 499] {
            assert!(
                !policy.should_retry_status(status),
                "{status} should not retry"
            );
        }
    }

    #[test]
    fn retry_on_header_replaces_set() {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_RETRY_ON, HeaderValue::from_static("429,503"));
        let policy = RetryPolicy::from_headers(&headers);
        assert!(policy.should_retry_status(429));
        assert!(policy.should_retry_status(503));
        assert!(!policy.should_retry_status(500));
        assert!(!policy.should_retry_status(408));
    }

    #[test]
    fn chain_from_header_overrides_default() {
        let default_chain = vec!["a/x".to_string()];
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_FALLBACKS,
            HeaderValue::from_static("groq/llama-3.3-70b, openai/gpt-4o-mini"),
        );
        assert_eq!(
            fallback_chain(&headers, &default_chain),
            vec!["groq/llama-3.3-70b", "openai/gpt-4o-mini"]
        );
    }

    #[test]
    fn validate_chain_accepts_and_normalizes_entries() {
        assert_eq!(validate_chain(&[]), Ok(vec![]));
        assert_eq!(
            validate_chain(&[" groq/llama-3.3-70b ".into(), "openai/gpt-4o".into()]),
            Ok(vec![
                "groq/llama-3.3-70b".to_string(),
                "openai/gpt-4o".to_string()
            ])
        );
    }

    #[test]
    fn validate_chain_rejects_empty_and_comma_entries() {
        assert!(validate_chain(&["  ".into()]).is_err());
        assert!(validate_chain(&["a/x,b/y".into()]).is_err());
    }

    fn descriptor(qualified_id: &str, context: u64) -> ModelDescriptor {
        ModelDescriptor {
            context: Some(LimitEvidence::new(
                context,
                EvidenceSource::Seeded,
                "test",
                0.9,
            )),
            ..ModelDescriptor::for_test(qualified_id)
        }
    }

    fn chain(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|&entry| entry.to_owned()).collect()
    }

    #[test]
    fn retain_eligible_drops_forbidden_fallbacks_and_keeps_order() {
        let snapshot = ModelDescriptorSnapshot::new(vec![
            descriptor("copilot/opus", 1_000_000),
            descriptor("copilot/gpt-6-sol", 1_000_000),
            descriptor("copilot/grok-fast-preview", 1_000_000),
            descriptor("copilot/haiku", 200_000),
        ]);
        let policy = SmartAutoPolicy {
            exclude: BTreeSet::from(["*fast-preview*".to_owned()]),
            min_context_tokens: Some(1_000_000),
            ..SmartAutoPolicy::default()
        };
        let eligible = SmartAutoPool::eligible(&snapshot, &policy, "/v1/messages", 0);
        let fallbacks = chain(&[
            "copilot/gpt-6-sol",
            "copilot/grok-fast-preview",
            "copilot/haiku",
            "copilot/auto",
            "hub/smart-auto-fast",
            "openai/unknown",
            "copilot/opus[1m]",
        ]);

        assert_eq!(
            retain_eligible(fallbacks, Some(&eligible)),
            ["copilot/gpt-6-sol", "copilot/opus[1m]"]
        );
    }

    #[test]
    fn retain_eligible_with_an_empty_pool_drops_every_fallback() {
        assert!(retain_eligible(chain(&["copilot/opus"]), Some(&[])).is_empty());
    }

    #[test]
    fn non_hub_chain_is_unchanged() {
        let default_chain = chain(&["copilot/auto", "hub/smart-auto", "a/x"]);
        let fallbacks = fallback_chain(&HeaderMap::new(), &default_chain);

        assert_eq!(retain_eligible(fallbacks, None), default_chain);
    }

    #[test]
    fn alias_header_and_default_chain() {
        let default_chain = vec!["a/x".to_string()];
        assert_eq!(
            fallback_chain(&HeaderMap::new(), &default_chain),
            vec!["a/x"]
        );
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_FALLBACKS_ALIAS, HeaderValue::from_static("b/y"));
        assert_eq!(fallback_chain(&headers, &default_chain), vec!["b/y"]);
    }
}

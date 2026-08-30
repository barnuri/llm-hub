use std::collections::HashSet;

use axum::http::HeaderMap;

use crate::consts::{HEADER_FALLBACKS, HEADER_FALLBACKS_ALIAS, HEADER_RETRY_ON, HEADER_TIMEOUT_MS};

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
    /// an explicit override wins, including over NEVER_RETRY.
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

pub fn per_attempt_timeout_ms(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, HEADER_TIMEOUT_MS)?.trim().parse().ok()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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

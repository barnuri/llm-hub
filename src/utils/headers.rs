use axum::http::header::HeaderMap;

use crate::consts::{
    HEADER_FALLBACKS, HEADER_FALLBACKS_ALIAS, HEADER_REASONING_STRIP, HEADER_RETRY_ON,
    HEADER_SYSTEM_PROMPT_MODE, HEADER_TIMEOUT_MS,
};
use crate::schemas::smart_auto_headers::SmartAutoHeaders;

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers that must never be forwarded upstream on top of hop-by-hop ones:
/// the caller's auth (replaced by the profile key), host/length (set by the
/// client lib), and llm-hub control headers.
const REQUEST_ONLY_STRIP: [&str; 11] = [
    "authorization",
    "x-api-key",
    "host",
    "content-length",
    HEADER_FALLBACKS,
    HEADER_FALLBACKS_ALIAS,
    HEADER_TIMEOUT_MS,
    HEADER_RETRY_ON,
    HEADER_SYSTEM_PROMPT_MODE,
    HEADER_REASONING_STRIP,
    crate::schemas::launch::profile_filter::ProfileFilter::HEADER,
];

/// Header name prefixes that are never forwarded upstream, for llm-hub control header families.
const REQUEST_ONLY_STRIP_PREFIXES: [&str; 1] = [SmartAutoHeaders::PREFIX];

pub fn filter_request_headers(headers: &HeaderMap) -> HeaderMap {
    filter(headers, &REQUEST_ONLY_STRIP, &REQUEST_ONLY_STRIP_PREFIXES)
}

pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    filter(headers, &["content-length"], &[])
}

fn filter(headers: &HeaderMap, extra_strip: &[&str], strip_prefixes: &[&str]) -> HeaderMap {
    let connection_named: Vec<String> = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').map(|s| s.trim().to_lowercase()).collect())
        .unwrap_or_default();

    let mut filtered = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.as_str().to_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        if extra_strip.contains(&lower.as_str()) {
            continue;
        }
        if strip_prefixes
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        {
            continue;
        }
        if connection_named.contains(&lower) {
            continue;
        }
        filtered.append(name.clone(), value.clone());
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                k.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn strips_hop_by_hop_and_auth_on_requests() {
        let out = filter_request_headers(&headers(&[
            ("authorization", "Bearer caller-key"),
            ("transfer-encoding", "chunked"),
            ("x-llm-hub-fallbacks", "a/b"),
            ("x-llm-hub-profiles", "copilot"),
            ("content-type", "application/json"),
        ]));
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("content-type"));
    }

    #[test]
    fn strips_every_smart_auto_header_on_requests() {
        let out = filter_request_headers(&headers(&[
            (SmartAutoHeaders::TIER, "balance"),
            ("X-LLM-Hub-Smart-Auto-Min-Context", "1m"),
            (SmartAutoHeaders::WORKER, "run/node/1"),
            ("x-llm-hub-smart-auto-unknown", "1"),
            ("x-llm-hub-smart-autopilot", "kept"),
            ("content-type", "application/json"),
        ]));
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("content-type"));
        assert!(out.contains_key("x-llm-hub-smart-autopilot"));
    }

    #[test]
    fn response_keeps_smart_auto_headers() {
        let out = filter_response_headers(&headers(&[(SmartAutoHeaders::TIER, "balance")]));
        assert!(out.contains_key(SmartAutoHeaders::TIER));
    }

    #[test]
    fn strips_connection_named_headers() {
        let out = filter_request_headers(&headers(&[
            ("connection", "x-custom, keep-alive"),
            ("x-custom", "1"),
            ("accept", "text/event-stream"),
        ]));
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("accept"));
    }

    #[test]
    fn response_keeps_content_type_drops_transfer_encoding() {
        let out = filter_response_headers(&headers(&[
            ("transfer-encoding", "chunked"),
            ("content-type", "text/event-stream"),
            ("x-request-id", "abc"),
        ]));
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("content-type"));
        assert!(out.contains_key("x-request-id"));
    }
}

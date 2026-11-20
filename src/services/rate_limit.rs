//! Reads when an upstream rate limit resets, and whether a rejection means the account is out of
//! credits. Headers only on the request path; a body only when the response phase already read it.

use std::time::Duration;

use axum::http::HeaderMap;
use serde_json::Value;

use crate::utils::time::{parse_http_date_ms, parse_rfc3339_ms};

/// The longest cooldown an upstream hint can set.
pub const HINT_CAP: Duration = Duration::from_hours(6);

/// The per-kind reset headers of `OpenAI`-style upstreams, with their remaining-count header.
const OPENAI_RESETS: [(&str, &str); 2] = [
    (
        "x-ratelimit-reset-requests",
        "x-ratelimit-remaining-requests",
    ),
    ("x-ratelimit-reset-tokens", "x-ratelimit-remaining-tokens"),
];

/// The per-kind reset headers of Anthropic, with their remaining-count header.
const ANTHROPIC_RESETS: [(&str, &str); 4] = [
    (
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-requests-remaining",
    ),
    (
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-tokens-remaining",
    ),
    (
        "anthropic-ratelimit-input-tokens-reset",
        "anthropic-ratelimit-input-tokens-remaining",
    ),
    (
        "anthropic-ratelimit-output-tokens-reset",
        "anthropic-ratelimit-output-tokens-remaining",
    ),
];

/// Error codes that mean the account has no credit left, not a short rate limit.
const QUOTA_CODES: [&str; 6] = [
    "insufficient_quota",
    "billing_hard_limit_reached",
    "billing_not_active",
    "insufficient_credits",
    "credits_exhausted",
    "payment_required",
];

/// Lowercase message parts that mean the account has no credit left.
const QUOTA_PHRASES: [&str; 6] = [
    "credit balance is too low",
    "insufficient credit",
    "out of credits",
    "insufficient_quota",
    "check your plan and billing details",
    "payment required",
];

/// Which models a cooldown covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownScope {
    /// The model that answered, on the route it answered on.
    Model,
    /// Every model of the profile.
    Profile,
}

/// A cooldown a built-in extension read from an error body, for the attempt loop to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CooldownHint {
    pub scope: CooldownScope,
    /// `None` means the configured out-of-credits cooldown.
    pub wait: Option<Duration>,
    pub reason: String,
}

/// What an error body says about the limit it hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyHint {
    /// The account has no credit left. Holds the upstream message.
    OutOfCredits(String),
    /// The limit resets after this wait.
    RetryAfter(Duration),
}

/// How long until the rate limit of a reply resets, from its headers, and the header that said
/// so. `None` when no header says. `now_ms` is the current Unix time in milliseconds.
#[must_use]
pub fn reset_after(headers: &HeaderMap, now_ms: i64) -> Option<(Duration, &'static str)> {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(ms) = text("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some((capped_secs(ms / 1_000.0)?, "retry-after-ms"));
    }
    if let Some(value) = text("retry-after") {
        let wait = match value.trim().parse::<f64>() {
            Ok(secs) => capped_secs(secs),
            Err(_) => parse_http_date_ms(value).map(|at| until(at, now_ms)),
        };
        if let Some(wait) = wait {
            return Some((wait, "retry-after"));
        }
    }
    if let Some(wait) = family_reset(headers, &OPENAI_RESETS, parse_duration) {
        return Some((wait, "x-ratelimit-reset"));
    }
    family_reset(headers, &ANTHROPIC_RESETS, |v| {
        parse_rfc3339_ms(v).map(|at| until(at, now_ms))
    })
    .map(|wait| (wait, "anthropic-ratelimit-reset"))
}

/// The reset of the exhausted kinds of one header family. When no remaining count says 0,
/// the longest reset of the family, so the hub never retries before every limit has room.
fn family_reset(
    headers: &HeaderMap,
    family: &[(&str, &str)],
    parse: impl Fn(&str) -> Option<Duration>,
) -> Option<Duration> {
    let mut exhausted: Option<Duration> = None;
    let mut longest: Option<Duration> = None;
    for (reset, remaining) in family {
        let Some(wait) = headers
            .get(*reset)
            .and_then(|value| value.to_str().ok())
            .and_then(&parse)
        else {
            continue;
        };
        longest = longest.max(Some(wait));
        let empty = headers
            .get(*remaining)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|count| count.trim() == "0");
        if empty {
            exhausted = exhausted.max(Some(wait));
        }
    }
    exhausted.or(longest)
}

/// What an error body of `status` says about the limit it hit, if anything.
#[must_use]
pub fn body_hint(status: u16, body: &Value) -> Option<BodyHint> {
    if !matches!(status, 400 | 402 | 403 | 429) {
        return None;
    }
    let message = crate::ext::error_message_of(body).unwrap_or_default();
    let code = ["/error/code", "/error/type", "/code"]
        .iter()
        .find_map(|pointer| body.pointer(pointer).and_then(Value::as_str))
        .unwrap_or_default();
    if status == 402 || QUOTA_CODES.contains(&code) {
        return Some(BodyHint::OutOfCredits(short(message, code)));
    }
    if status == 429
        && let Some(wait) = body_retry_delay(body, message)
    {
        return Some(BodyHint::RetryAfter(wait));
    }
    let lower = message.to_ascii_lowercase();
    QUOTA_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
        .then(|| BodyHint::OutOfCredits(short(message, code)))
}

/// Gemini's `error.details[].retryDelay`, or "try again in 6m0s" in the message.
fn body_retry_delay(body: &Value, message: &str) -> Option<Duration> {
    let detail = body
        .pointer("/error/details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|detail| detail.get("retryDelay").and_then(Value::as_str));
    if let Some(wait) = detail.and_then(parse_duration) {
        return Some(wait);
    }
    let lower = message.to_ascii_lowercase();
    let (_, after) = lower.split_once("try again in ")?;
    let mut words = after.split_whitespace();
    let token = words.next()?.trim_end_matches(['.', ',']);
    if let Ok(amount) = token.parse::<f64>() {
        let unit = words.next().unwrap_or("s");
        let secs = if unit.starts_with("min") {
            amount * 60.0
        } else if unit.starts_with("hour") {
            amount * 3_600.0
        } else if unit.starts_with("ms") || unit.starts_with("milli") {
            amount / 1_000.0
        } else {
            amount
        };
        return capped_secs(secs);
    }
    parse_duration(token)
}

/// A Go-style duration (`6m0s`, `1.5s`, `20ms`, `1h2m`), or plain seconds (`30`), capped.
#[must_use]
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(secs) = text.parse::<f64>() {
        return capped_secs(secs);
    }
    let mut total = 0.0;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .filter(|&end| end > 0)?;
        let amount: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "h" => 3_600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 1e-3,
            "us" | "µs" => 1e-6,
            "ns" => 1e-9,
            _ => return None,
        };
        total += amount * scale;
        rest = &rest[unit_len..];
    }
    capped_secs(total)
}

fn capped_secs(secs: f64) -> Option<Duration> {
    (secs.is_finite() && secs >= 0.0)
        .then(|| Duration::from_secs_f64(secs.min(HINT_CAP.as_secs_f64())))
}

/// The wait from `now_ms` to `at_ms`, capped. A time in the past is no wait.
fn until(at_ms: i64, now_ms: i64) -> Duration {
    let left = u64::try_from(at_ms.saturating_sub(now_ms)).unwrap_or(0);
    Duration::from_millis(left).min(HINT_CAP)
}

/// The message for a log line, at most 160 characters. Falls back to the code.
fn short(message: &str, code: &str) -> String {
    let text = if message.is_empty() { code } else { message };
    text.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    const NOW_MS: i64 = 1_790_792_466_000;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn retry_after_takes_seconds_milliseconds_or_a_date() {
        assert_eq!(
            reset_after(&headers(&[("retry-after", "120")]), NOW_MS),
            Some((Duration::from_mins(2), "retry-after"))
        );
        assert_eq!(
            reset_after(
                &headers(&[("retry-after-ms", "1500"), ("retry-after", "9")]),
                NOW_MS
            ),
            Some((Duration::from_millis(1_500), "retry-after-ms"))
        );
        let date = headers(&[("retry-after", "Wed, 30 Sep 2026 18:22:06 GMT")]);
        assert_eq!(
            reset_after(&date, NOW_MS),
            Some((Duration::from_mins(1), "retry-after"))
        );
        let past = headers(&[("retry-after", "Wed, 30 Sep 2026 18:00:00 GMT")]);
        assert_eq!(
            reset_after(&past, NOW_MS),
            Some((Duration::ZERO, "retry-after"))
        );
    }

    #[test]
    fn a_long_hint_is_capped_and_garbage_is_ignored() {
        assert_eq!(
            reset_after(&headers(&[("retry-after", "999999")]), NOW_MS),
            Some((HINT_CAP, "retry-after"))
        );
        assert_eq!(
            reset_after(&headers(&[("retry-after", "soon")]), NOW_MS),
            None
        );
        assert_eq!(reset_after(&HeaderMap::new(), NOW_MS), None);
    }

    #[test]
    fn openai_resets_use_the_exhausted_kind() {
        let map = headers(&[
            ("x-ratelimit-reset-requests", "6m0s"),
            ("x-ratelimit-remaining-requests", "10"),
            ("x-ratelimit-reset-tokens", "1.5s"),
            ("x-ratelimit-remaining-tokens", "0"),
        ]);
        assert_eq!(
            reset_after(&map, NOW_MS),
            Some((Duration::from_millis(1_500), "x-ratelimit-reset"))
        );
        let unknown = headers(&[
            ("x-ratelimit-reset-requests", "6m0s"),
            ("x-ratelimit-reset-tokens", "20ms"),
        ]);
        assert_eq!(
            reset_after(&unknown, NOW_MS),
            Some((Duration::from_mins(6), "x-ratelimit-reset"))
        );
    }

    #[test]
    fn anthropic_resets_are_timestamps() {
        let map = headers(&[
            ("anthropic-ratelimit-tokens-reset", "2026-09-30T18:21:36Z"),
            ("anthropic-ratelimit-tokens-remaining", "0"),
            ("anthropic-ratelimit-requests-reset", "2026-09-30T18:25:06Z"),
        ]);
        assert_eq!(
            reset_after(&map, NOW_MS),
            Some((Duration::from_secs(30), "anthropic-ratelimit-reset"))
        );
    }

    #[test]
    fn durations_parse_in_go_and_plain_forms() {
        assert_eq!(parse_duration("6m0s"), Some(Duration::from_mins(6)));
        assert_eq!(parse_duration("1h2m3s"), Some(Duration::from_secs(3_723)));
        assert_eq!(parse_duration("20ms"), Some(Duration::from_millis(20)));
        assert_eq!(parse_duration("13s"), Some(Duration::from_secs(13)));
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
        for bad in ["", "s", "5x", "-1", "1.2.3s"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn out_of_credit_bodies_are_recognised() {
        let openai = json!({"error": {"message": "You exceeded your current quota, please check your plan and billing details.", "code": "insufficient_quota"}});
        assert!(matches!(
            body_hint(429, &openai),
            Some(BodyHint::OutOfCredits(m)) if m.starts_with("You exceeded")
        ));
        let anthropic = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Your credit balance is too low to access the Anthropic API."}});
        assert!(matches!(
            body_hint(400, &anthropic),
            Some(BodyHint::OutOfCredits(_))
        ));
        assert_eq!(
            body_hint(402, &json!({"error": {"code": "payment_required"}})),
            Some(BodyHint::OutOfCredits("payment_required".into()))
        );
    }

    #[test]
    fn retry_delays_in_bodies_win_over_quota_words_on_a_429() {
        let gemini = json!({"error": {"code": 429, "message": "You exceeded your current quota, please check your plan and billing details.", "details": [{"@type": "x"}, {"retryDelay": "13s"}]}});
        assert_eq!(
            body_hint(429, &gemini),
            Some(BodyHint::RetryAfter(Duration::from_secs(13)))
        );
        let openai = json!({"error": {"message": "Rate limit reached. Please try again in 6m0s.", "code": "rate_limit_exceeded"}});
        assert_eq!(
            body_hint(429, &openai),
            Some(BodyHint::RetryAfter(Duration::from_mins(6)))
        );
        let words = json!({"error": {"message": "Please try again in 2 minutes"}});
        assert_eq!(
            body_hint(429, &words),
            Some(BodyHint::RetryAfter(Duration::from_mins(2)))
        );
        let hours = json!({"error": {"message": "try again in 1 hour"}});
        assert_eq!(
            body_hint(429, &hours),
            Some(BodyHint::RetryAfter(Duration::from_hours(1)))
        );
        let millis = json!({"error": {"message": "try again in 500 ms"}});
        assert_eq!(
            body_hint(429, &millis),
            Some(BodyHint::RetryAfter(Duration::from_millis(500)))
        );
    }

    #[test]
    fn ordinary_errors_give_no_hint() {
        let schema = json!({"error": {"message": "messages: field required", "type": "invalid_request_error"}});
        assert_eq!(body_hint(400, &schema), None);
        assert_eq!(
            body_hint(429, &json!({"error": {"message": "slow down"}})),
            None
        );
        assert_eq!(
            body_hint(500, &json!({"error": {"code": "insufficient_quota"}})),
            None
        );
        assert_eq!(
            body_hint(429, &json!({"error": {"message": "try again in a bit"}})),
            None
        );
    }
}

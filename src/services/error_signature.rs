//! Normalized error signatures, so repeats of one failure group together.
//!
//! The hub computes signatures when it reads errors, never on the request path.

use std::collections::HashMap;

use crate::schemas::errors_report::{ErrorGroup, NameCount};

/// Longest signature kept, in characters.
pub const MAX_SIGNATURE_CHARS: usize = 200;
/// Names listed per group for profiles and models.
const TOP_NAMES: usize = 3;
/// Shortest digit run that becomes `<n>`; shorter ones (status codes, indexes) stay.
const LONG_NUMBER_DIGITS: usize = 4;
/// Shortest hex run (with a digit) that becomes `<id>`.
const HEX_ID_CHARS: usize = 8;
/// Shortest mixed letter and digit run that becomes `<id>`.
const OPAQUE_ID_CHARS: usize = 12;

/// Signature of one failure: the first non-empty line of `error` with ANSI codes,
/// timestamps, ids, long numbers and URL query strings replaced by placeholders.
/// An empty or missing reason falls back to the HTTP status.
#[must_use]
pub fn error_signature(error: Option<&str>, status: u16) -> String {
    let line = error
        .map(strip_ansi)
        .and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(normalize_line)
        })
        .unwrap_or_default();
    if line.is_empty() {
        format!("HTTP {status} (no reason recorded)")
    } else {
        line
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>' | ']' | ',' | '`')
}

fn normalize_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_url = false;
    while i < chars.len() {
        let c = chars[i];
        if in_url && (c == '?' || c == '#') {
            while i < chars.len() && !ends_url(chars[i]) {
                i += 1;
            }
            in_url = false;
            continue;
        }
        if in_url && ends_url(c) {
            in_url = false;
        }
        if !in_url && (starts_with(&chars[i..], "http://") || starts_with(&chars[i..], "https://"))
        {
            in_url = true;
        }
        if let Some(len) = timestamp_len(&chars[i..]) {
            out.push_str("<ts>");
            i += len;
            continue;
        }
        if is_word_char(c) {
            let start = i;
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let after_field =
                start >= 2 && chars[start - 1] == '.' && chars[start - 2].is_ascii_alphabetic();
            if after_field && word.chars().all(|c| c.is_ascii_digit()) {
                // `messages.27.content` and `messages.1.content` are one field path.
                out.push('#');
            } else {
                out.push_str(&normalize_word(&word));
            }
            continue;
        }
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    let trimmed = out.trim();
    match trimmed.char_indices().nth(MAX_SIGNATURE_CHARS) {
        Some((cut, _)) => trimmed[..cut].trim_end().to_string(),
        None => trimmed.to_string(),
    }
}

fn starts_with(chars: &[char], prefix: &str) -> bool {
    let mut it = chars.iter();
    prefix.chars().all(|p| it.next() == Some(&p))
}

/// Length of an ISO date-time (`2026-11-20T10:11:12.5Z`) or clock (`10:11:12`) at the start.
fn timestamp_len(chars: &[char]) -> Option<usize> {
    let digits =
        |at: usize, n: usize| (at..at + n).all(|k| chars.get(k).is_some_and(char::is_ascii_digit));
    let is = |at: usize, c: char| chars.get(at) == Some(&c);
    let date = digits(0, 4) && is(4, '-') && digits(5, 2) && is(7, '-') && digits(8, 2);
    let (mut end, has_time) = if date {
        if (is(10, 'T') || is(10, ' ')) && digits(11, 2) && is(13, ':') && digits(14, 2) {
            (if is(16, ':') && digits(17, 2) { 19 } else { 16 }, true)
        } else {
            (10, false)
        }
    } else if digits(0, 2) && is(2, ':') && digits(3, 2) && is(5, ':') && digits(6, 2) {
        (8, true)
    } else {
        return None;
    };
    if has_time {
        if is(end, '.') && digits(end + 1, 1) {
            end += 1;
            while chars.get(end).is_some_and(char::is_ascii_digit) {
                end += 1;
            }
        }
        if is(end, 'Z') {
            return Some(end + 1);
        }
        if (is(end, '+') || is(end, '-')) && digits(end + 1, 2) {
            end += 3;
            if is(end, ':') && digits(end + 1, 2) {
                end += 3;
            } else if digits(end, 2) {
                end += 2;
            }
        }
    }
    // A longer run (`2026-11-201`) is not a date.
    if chars.get(end).is_some_and(char::is_ascii_alphanumeric) {
        return None;
    }
    Some(end)
}

fn normalize_word(word: &str) -> String {
    if word.chars().all(|c| c.is_ascii_digit()) {
        return if word.len() >= LONG_NUMBER_DIGITS {
            "<n>".into()
        } else {
            word.into()
        };
    }
    if is_id(word) {
        return "<id>".into();
    }
    if let Some(sep) = word.find(['-', '_']) {
        let (prefix, rest) = word.split_at(sep);
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphabetic())
            && is_id(&rest[1..])
        {
            return format!("{prefix}{}<id>", &rest[..1]);
        }
    }
    word.into()
}

/// UUIDs, hex runs (hashes) and opaque mixed tokens (`01HXYZabc123`) with a digit.
fn is_id(word: &str) -> bool {
    if !word.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    let parts: Vec<usize> = word.split('-').map(str::len).collect();
    let all_hex = word.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if all_hex && (parts == [8, 4, 4, 4, 12] || parts.len() == 1 && word.len() >= HEX_ID_CHARS) {
        return true;
    }
    word.len() >= OPAQUE_ID_CHARS
        && word.chars().all(|c| c.is_ascii_alphanumeric())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// Failed rows that share an exact reason, status, model and profile in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorTally {
    pub error: Option<String>,
    pub status: u16,
    pub model: String,
    pub profile: String,
    pub count: u64,
    pub open: u64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
}

#[derive(Default)]
struct GroupAcc {
    count: u64,
    open: u64,
    first_ts_ms: u64,
    last_ts_ms: u64,
    sample: Option<String>,
    sample_status: u16,
    profiles: HashMap<String, u64>,
    models: HashMap<String, u64>,
}

/// Merges tallies by signature, largest group first, newest first on a tie.
#[must_use]
pub fn group_errors(tallies: &[ErrorTally], max_groups: usize) -> Vec<ErrorGroup> {
    let mut groups: HashMap<String, GroupAcc> = HashMap::new();
    for tally in tallies {
        let acc = groups
            .entry(error_signature(tally.error.as_deref(), tally.status))
            .or_default();
        if acc.count == 0 || tally.first_ts_ms < acc.first_ts_ms {
            acc.first_ts_ms = tally.first_ts_ms;
        }
        if acc.count == 0 || tally.last_ts_ms >= acc.last_ts_ms {
            acc.last_ts_ms = tally.last_ts_ms;
            acc.sample.clone_from(&tally.error);
            acc.sample_status = tally.status;
        }
        acc.count += tally.count;
        acc.open += tally.open;
        *acc.profiles.entry(tally.profile.clone()).or_default() += tally.count;
        *acc.models.entry(tally.model.clone()).or_default() += tally.count;
    }
    let mut out: Vec<ErrorGroup> = groups
        .into_iter()
        .map(|(signature, acc)| ErrorGroup {
            signature,
            count: acc.count,
            open: acc.open,
            resolved: acc.count.saturating_sub(acc.open),
            first_ts_ms: acc.first_ts_ms,
            last_ts_ms: acc.last_ts_ms,
            profile_count: acc.profiles.len(),
            model_count: acc.models.len(),
            profiles: top_names(acc.profiles),
            models: top_names(acc.models),
            sample: acc.sample,
            sample_status: acc.sample_status,
        })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.last_ts_ms.cmp(&a.last_ts_ms))
            .then_with(|| a.signature.cmp(&b.signature))
    });
    out.truncate(max_groups);
    out
}

fn top_names(names: HashMap<String, u64>) -> Vec<NameCount> {
    let mut list: Vec<NameCount> = names
        .into_iter()
        .map(|(name, count)| NameCount { name, count })
        .collect();
    list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    list.truncate(TOP_NAMES);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(text: &str) -> String {
        error_signature(Some(text), 500)
    }

    #[test]
    fn plain_upstream_reasons_stay_as_they_are() {
        assert_eq!(sig("Request Entity Too Large"), "Request Entity Too Large");
        assert_eq!(
            sig(
                "image input is not supported - hint: if this is unexpected, you may need to provide the mmproj"
            ),
            "image input is not supported - hint: if this is unexpected, you may need to provide the mmproj"
        );
        assert_eq!(
            sig("messages.27.content.0: Invalid `signature` in `thinking` block"),
            "messages.#.content.#: Invalid `signature` in `thinking` block"
        );
        assert_eq!(
            sig("HTTP 404 with empty body from upstream"),
            "HTTP 404 with empty body from upstream"
        );
    }

    #[test]
    fn model_names_and_short_numbers_survive() {
        assert_eq!(
            sig("model \"gpt-4o-mini-2024-07-18\" failed: got 8, expected >= 16"),
            "model \"gpt-4o-mini-2024-07-18\" failed: got 8, expected >= 16"
        );
        assert_eq!(
            sig("eu.anthropic.claude-sonnet-5-5 is busy"),
            "eu.anthropic.claude-sonnet-5-5 is busy"
        );
        assert_eq!(
            sig("model=gpt-4.1-2025-04-14 and [lfm2.5-8b-a1b] are not ready"),
            "model=gpt-4.1-2025-04-14 and [lfm2.5-8b-a1b] are not ready"
        );
        assert_eq!(sig("host 127.0.0.1 is down"), "host 127.0.0.1 is down");
    }

    #[test]
    fn long_numbers_merge_token_limit_errors() {
        let a = sig("prompt token count of 1150005 exceeds the limit of 922000");
        let b = sig("prompt token count of 2300066 exceeds the limit of 372000");
        assert_eq!(a, "prompt token count of <n> exceeds the limit of <n>");
        assert_eq!(a, b);
    }

    #[test]
    fn connect_errors_drop_query_strings_and_ports() {
        let a = sig(
            "connect error: error sending request for url (http://127.0.0.1:11436/v1/chat/completions?api-version=2024-10-01&x=1)",
        );
        let b = sig(
            "connect error: error sending request for url (http://127.0.0.1:11440/v1/chat/completions)",
        );
        assert_eq!(
            a,
            "connect error: error sending request for url (http://127.0.0.1:<n>/v1/chat/completions)"
        );
        assert_eq!(a, b);
        assert_eq!(
            sig("GET https://api.example.com/v1/models#frag failed"),
            "GET https://api.example.com/v1/models failed"
        );
    }

    #[test]
    fn request_ids_uuids_and_hex_become_placeholders() {
        let a = sig(
            "via gateway · request_id=c4794ee6-f60c-4445-b4e5-77a7b50e139a: Bedrock is unable to process your request.",
        );
        let b = sig(
            "via gateway · request_id=3b56637d-8e09-4034-a050-f807d8f79539: Bedrock is unable to process your request.",
        );
        assert_eq!(
            a,
            "via gateway · request_id=<id>: Bedrock is unable to process your request."
        );
        assert_eq!(a, b);
        assert_eq!(sig("trace deadbeef01234567 lost"), "trace <id> lost");
        assert_eq!(sig("req-9f8e7d6c5b4a broke"), "req-<id> broke");
        assert_eq!(
            sig("call toolu_01AbCdEfGh2345XyZ failed"),
            "call toolu_<id> failed"
        );
    }

    #[test]
    fn json_bodies_with_ids_and_timestamps_group() {
        let a = sig(
            r#"{"error":{"message":"Rate limited","id":"chatcmpl-9xQ2abcDEF7890ghij","created":"2026-11-20T10:11:12.345Z"}}"#,
        );
        let b = sig(
            r#"{"error":{"message":"Rate limited","id":"chatcmpl-7yZ3klmNOP1234qrst","created":"2026-11-19 08:00:01+02:00"}}"#,
        );
        assert_eq!(
            a,
            r#"{"error":{"message":"Rate limited","id":"chatcmpl-<id>","created":"<ts>"}}"#
        );
        assert_eq!(a, b);
    }

    #[test]
    fn timestamps_in_every_shape_are_replaced() {
        assert_eq!(sig("at 2026-11-20 failed"), "at <ts> failed");
        assert_eq!(sig("at 2026-11-20T10:11 failed"), "at <ts> failed");
        assert_eq!(sig("[10:11:12.123] timeout"), "[<ts>] timeout");
        assert_eq!(sig("at 2026-11-20T10:11:12+0200 failed"), "at <ts> failed");
        assert_eq!(sig("build 2026-11-201 bad"), "build 2026-11-201 bad");
        assert_eq!(sig("at 10:11:12Z"), "at <ts>");
    }

    #[test]
    fn ansi_codes_extra_lines_and_spaces_are_dropped() {
        assert_eq!(
            sig("\n  \u{1b}[31mError:\u{1b}[0m   upstream   closed\n    at frame 1\n"),
            "Error: upstream closed"
        );
    }

    #[test]
    fn missing_or_blank_reason_falls_back_to_status() {
        assert_eq!(error_signature(None, 400), "HTTP 400 (no reason recorded)");
        assert_eq!(
            error_signature(Some("  \n "), 404),
            "HTTP 404 (no reason recorded)"
        );
    }

    #[test]
    fn signatures_cap_at_the_char_limit() {
        let long = "é".repeat(MAX_SIGNATURE_CHARS + 50);
        assert_eq!(sig(&long).chars().count(), MAX_SIGNATURE_CHARS);
    }

    fn tally(
        error: &str,
        model: &str,
        profile: &str,
        count: u64,
        open: u64,
        ts: u64,
    ) -> ErrorTally {
        ErrorTally {
            error: Some(error.into()),
            status: 400,
            model: model.into(),
            profile: profile.into(),
            count,
            open,
            first_ts_ms: ts,
            last_ts_ms: ts + 10,
        }
    }

    #[test]
    fn groups_merge_by_signature_and_rank_by_count() {
        let tallies = [
            tally(
                "prompt token count of 1150005 exceeds the limit of 922000",
                "m1",
                "a",
                2,
                1,
                100,
            ),
            tally(
                "prompt token count of 2300066 exceeds the limit of 372000",
                "m2",
                "b",
                3,
                3,
                50,
            ),
            tally("Request Entity Too Large", "m1", "a", 1, 0, 500),
        ];
        let groups = group_errors(&tallies, 10);

        assert_eq!(groups.len(), 2);
        let top = &groups[0];
        assert_eq!(
            top.signature,
            "prompt token count of <n> exceeds the limit of <n>"
        );
        assert_eq!((top.count, top.open, top.resolved), (5, 4, 1));
        assert_eq!((top.first_ts_ms, top.last_ts_ms), (50, 110));
        assert_eq!(
            top.sample.as_deref(),
            Some("prompt token count of 1150005 exceeds the limit of 922000")
        );
        assert_eq!(
            top.models[0],
            NameCount {
                name: "m2".into(),
                count: 3
            }
        );
        assert_eq!((top.model_count, top.profile_count), (2, 2));
        assert_eq!(groups[1].resolved, 1);
    }

    #[test]
    fn groups_cap_names_and_group_count() {
        let tallies: Vec<ErrorTally> = (0..5)
            .map(|k| tally(&format!("boom {k}"), &format!("m{k}"), "a", 1, 1, 10))
            .chain((0..5).map(|k| tally("same", &format!("m{k}"), &format!("p{k}"), 1, 1, 10)))
            .collect();
        let groups = group_errors(&tallies, 2);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].signature, "same");
        assert_eq!(groups[0].models.len(), TOP_NAMES);
        assert_eq!(groups[0].model_count, 5);
    }
}

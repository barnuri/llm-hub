use crate::consts::{CONTEXT_1M_SUFFIX, CONTEXT_TOKENS_1M, CONTEXT_TOKENS_200K};
use crate::schemas::evidence_source::EvidenceSource;
use crate::schemas::limit_evidence::LimitEvidence;
use serde_json::Value;

/// Splits a trailing window suffix, `[<n>k]` or `[<n>m]` (case-insensitive), off a model id.
/// `[200k]` is 200,000 tokens and `[1m]` is 1,000,000. Returns the bare id and the window.
/// Text that is not a window suffix stays in the id, so `[abc]` or `[0m]` leave it unchanged.
pub fn strip_window_suffix(raw: &str) -> (&str, Option<u64>) {
    let trimmed = raw.trim();
    let Some((bare, inner)) = trimmed
        .strip_suffix(']')
        .and_then(|rest| rest.rsplit_once('['))
    else {
        return (trimmed, None);
    };
    match parse_window(inner) {
        Some(tokens) => (bare, Some(tokens)),
        None => (trimmed, None),
    }
}

/// The token count of a window such as `200k` or `1M`. Digits only, then `k` or `m`.
fn parse_window(raw: &str) -> Option<u64> {
    let (&unit, digits) = raw.as_bytes().split_last()?;
    let scale = match unit.to_ascii_lowercase() {
        b'k' => 1_000,
        b'm' => 1_000_000,
        _ => return None,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let count: u64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    count.checked_mul(scale).filter(|tokens| *tokens > 0)
}

/// Read a context window from an upstream `/v1/models` entry. llama-swap
/// has no top-level field for it; its per-model `metadata:` config block is
/// surfaced as `meta.llamaswap`, so that is checked as a fallback, and so is
/// Copilot's `capabilities.limits.max_prompt_tokens`.
pub fn max_input_tokens_from_item(item: &Value) -> Option<u64> {
    input_limit_evidence_from_item(item, "").map(|evidence| evidence.value)
}

/// Confidence in a value an upstream lists in `/models`, which it may not enforce.
pub const LISTED_CONFIDENCE: f32 = 0.5;
/// Confidence in a context size the local inference server allocated.
pub const LOCAL_RUNTIME_CONFIDENCE: f32 = 0.9;

/// [`max_input_tokens_from_item`] with its provenance: llama-swap `meta.llamaswap`
/// is the local runtime allocation, every other field is a listed value.
pub fn input_limit_evidence_from_item(item: &Value, scope: &str) -> Option<LimitEvidence> {
    let listed = |value| (value, EvidenceSource::Listed, LISTED_CONFIDENCE);
    let (value, source, confidence) = context_tokens_in(item)
        .map(listed)
        .or_else(|| {
            llama_swap_context_tokens(item).map(|value| {
                (
                    value,
                    EvidenceSource::LocalRuntime,
                    LOCAL_RUNTIME_CONFIDENCE,
                )
            })
        })
        .or_else(|| {
            item.pointer("/capabilities/limits/max_prompt_tokens")
                .and_then(Value::as_u64)
                .filter(|tokens| *tokens > 0)
                .map(listed)
        })?;
    Some(LimitEvidence::new(value, source, scope, confidence))
}

/// The context size llama-swap reports from the model's `metadata:` config block.
pub fn llama_swap_context_tokens(item: &Value) -> Option<u64> {
    item.get("meta")
        .and_then(|meta| meta.get("llamaswap"))
        .and_then(context_tokens_in)
}

fn context_tokens_in(obj: &Value) -> Option<u64> {
    ["max_input_tokens", "context_length", "context_window"]
        .into_iter()
        .filter_map(|key| obj.get(key).and_then(Value::as_u64))
        .find(|tokens| *tokens > 0)
}

/// Window Claude Code should assume for this hub id.
///
/// Uses upstream `/v1/models` metadata only — no name guessing. The same
/// model family (e.g. Sonnet 5) can be listed at 200k and 1M as separate
/// upstream entries.
pub fn inferred_max_input_tokens(_model_id: &str, upstream_tokens: Option<u64>) -> Option<u64> {
    upstream_tokens.filter(|t| *t > 0)
}

/// The `/v1/models` entries for a qualified id, each with the window it declares. A model with a
/// 1M+ window lists its bare id at 200k and its `[1m]` id at 1M. Any other model lists one entry.
pub fn advertised_models(
    qualified: &str,
    upstream_tokens: Option<u64>,
) -> Vec<(String, Option<u64>)> {
    let tokens = inferred_max_input_tokens(qualified, upstream_tokens);
    if !tokens.is_some_and(|tokens| tokens >= CONTEXT_TOKENS_1M) {
        return vec![(qualified.to_string(), tokens)];
    }
    let bare = strip_window_suffix(qualified).0;
    vec![
        (bare.to_string(), Some(CONTEXT_TOKENS_200K)),
        (
            format!("{bare}{CONTEXT_1M_SUFFIX}"),
            Some(CONTEXT_TOKENS_1M),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llama_swap_metadata_context_length_read() {
        let item = serde_json::json!({
            "id": "qwen3.6-35b",
            "meta": { "llamaswap": { "context_length": 262_144, "type": "model" } }
        });
        assert_eq!(max_input_tokens_from_item(&item), Some(262_144));
    }

    #[test]
    fn top_level_context_wins_over_llama_swap_metadata() {
        let item = serde_json::json!({
            "context_length": 131_072,
            "meta": { "llamaswap": { "context_length": 262_144 } }
        });
        assert_eq!(max_input_tokens_from_item(&item), Some(131_072));
    }

    #[test]
    fn copilot_capabilities_max_prompt_tokens_read_as_fallback() {
        let item = serde_json::json!({
            "id": "claude-opus-4.5",
            "capabilities": { "limits": { "max_prompt_tokens": 128_000, "max_output_tokens": 16_000 } }
        });
        assert_eq!(max_input_tokens_from_item(&item), Some(128_000));
    }

    #[test]
    fn top_level_context_wins_over_copilot_capabilities() {
        let item = serde_json::json!({
            "context_length": 200_000,
            "capabilities": { "limits": { "max_prompt_tokens": 128_000 } }
        });
        assert_eq!(max_input_tokens_from_item(&item), Some(200_000));
    }

    #[test]
    fn trailing_window_suffix_stripped() {
        assert_eq!(
            strip_window_suffix("llmgw/bedrock/claude-opus-5[1m]"),
            ("llmgw/bedrock/claude-opus-5", Some(1_000_000))
        );
        assert_eq!(
            strip_window_suffix("llmgw/bedrock/claude-opus-5[1M]"),
            ("llmgw/bedrock/claude-opus-5", Some(1_000_000))
        );
        assert_eq!(
            strip_window_suffix("copilot/claude-sonnet-5-5[200k]"),
            ("copilot/claude-sonnet-5-5", Some(200_000))
        );
        assert_eq!(
            strip_window_suffix("copilot/claude-sonnet-5-5[400K]"),
            ("copilot/claude-sonnet-5-5", Some(400_000))
        );
    }

    #[test]
    fn text_that_is_not_a_window_suffix_stays() {
        for raw in [
            "llama_swap/meta/muse-glimmer",
            "local/model[abc]",
            "local/model[0m]",
            "local/model[k]",
            "local/model[1.5m]",
            "local/model[ 1m]",
            "local/model[1000000]",
            "local/model[1m]x",
        ] {
            assert_eq!(strip_window_suffix(raw), (raw, None), "{raw}");
        }
    }

    #[test]
    fn window_suffix_of_a_huge_count_does_not_overflow() {
        let raw = format!("local/model[{}m]", u64::MAX);
        assert_eq!(strip_window_suffix(&raw), (raw.as_str(), None));
    }

    #[test]
    fn upstream_1m_lists_bare_at_200k_and_suffixed_at_1m() {
        assert_eq!(
            advertised_models("llmgw/bedrock/anthropic.claude-opus-5", Some(1_000_000)),
            vec![
                (
                    "llmgw/bedrock/anthropic.claude-opus-5".to_string(),
                    Some(200_000)
                ),
                (
                    "llmgw/bedrock/anthropic.claude-opus-5[1m]".to_string(),
                    Some(1_000_000)
                ),
            ]
        );
        assert_eq!(
            advertised_models("llmgw/bedrock/anthropic.claude-sonnet-5", Some(1_000_000))[1].0,
            "llmgw/bedrock/anthropic.claude-sonnet-5[1m]"
        );
    }

    #[test]
    fn upstream_200k_lists_one_plain_entry_even_for_sonnet_5_name() {
        assert_eq!(
            advertised_models("llmgw/bedrock/anthropic.claude-sonnet-5", Some(200_000)),
            vec![(
                "llmgw/bedrock/anthropic.claude-sonnet-5".to_string(),
                Some(200_000)
            )]
        );
        assert_eq!(
            advertised_models("llmgw/bedrock/anthropic.claude-opus-5", Some(200_000)),
            vec![(
                "llmgw/bedrock/anthropic.claude-opus-5".to_string(),
                Some(200_000)
            )]
        );
    }

    #[test]
    fn missing_upstream_tokens_lists_one_plain_entry() {
        assert_eq!(
            advertised_models("llmgw/bedrock/anthropic.claude-sonnet-5", None),
            vec![("llmgw/bedrock/anthropic.claude-sonnet-5".to_string(), None)]
        );
        assert_eq!(
            inferred_max_input_tokens("llmgw/bedrock/anthropic.claude-opus-5", None),
            None
        );
    }

    #[test]
    fn upstream_tokens_passed_through() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/bedrock/anthropic.claude-opus-5", Some(1_000_000)),
            Some(1_000_000)
        );
    }

    #[test]
    fn already_suffixed_id_not_double_suffixed() {
        assert_eq!(
            advertised_models("llmgw/bedrock/claude-opus-5[1m]", Some(1_000_000))
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            [
                "llmgw/bedrock/claude-opus-5",
                "llmgw/bedrock/claude-opus-5[1m]"
            ]
        );
    }

    #[test]
    fn local_llama_id_no_inferred_window() {
        assert_eq!(
            inferred_max_input_tokens("llama_swap/meta/muse-glimmer", None),
            None
        );
        assert_eq!(
            advertised_models("llama_swap/meta/muse-glimmer", None),
            vec![("llama_swap/meta/muse-glimmer".to_string(), None)]
        );
    }

    #[test]
    fn claude_haiku_upstream_200k_lists_plain_id_at_200k() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/anthropic.claude-haiku-4-5", Some(200_000)),
            Some(200_000)
        );
        assert_eq!(
            advertised_models("llmgw/anthropic.claude-haiku-4-5", Some(200_000)),
            vec![(
                "llmgw/anthropic.claude-haiku-4-5".to_string(),
                Some(200_000)
            )]
        );
    }
}

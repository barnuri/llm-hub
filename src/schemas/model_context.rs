use crate::consts::{CONTEXT_1M_SUFFIX, CONTEXT_TOKENS_1M};
use serde_json::Value;

/// Drops a trailing `[1m]` / `[1M]` that Claude Code adds to declare a 1M window.
pub fn strip_1m_suffix(raw: &str) -> &str {
    let trimmed = raw.trim();
    let suffix_len = CONTEXT_1M_SUFFIX.len();
    let Some(prefix) = trimmed.get(..trimmed.len().saturating_sub(suffix_len)) else {
        return trimmed;
    };
    let suffix = &trimmed[prefix.len()..];
    if suffix.eq_ignore_ascii_case(CONTEXT_1M_SUFFIX) {
        prefix
    } else {
        trimmed
    }
}

/// True when `raw` already ends with Claude Code's 1M-window suffix.
pub fn has_1m_suffix(raw: &str) -> bool {
    strip_1m_suffix(raw) != raw.trim()
}

/// Read a context window from an upstream `/v1/models` entry.
pub fn max_input_tokens_from_item(item: &Value) -> Option<u64> {
    for key in ["max_input_tokens", "context_length", "context_window"] {
        if let Some(tokens) = item.get(key).and_then(Value::as_u64)
            && tokens > 0
        {
            return Some(tokens);
        }
    }
    None
}

/// Window Claude Code should assume for this hub id.
///
/// Prefer upstream metadata; fall back to name heuristics only when the
/// upstream omits token counts (static model lists, dumb proxies).
pub fn inferred_max_input_tokens(model_id: &str, upstream_tokens: Option<u64>) -> Option<u64> {
    if let Some(tokens) = upstream_tokens.filter(|t| *t > 0) {
        return Some(tokens);
    }
    looks_like_claude_1m(model_id).then_some(CONTEXT_TOKENS_1M)
}

/// Hub `/v1/models` id Claude Code should see. Models with a 1M+ window get
/// `[1m]` so the client does not default gateway ids to 200k.
pub fn advertised_model_id(qualified: &str, upstream_tokens: Option<u64>) -> String {
    if should_advertise_1m(qualified, upstream_tokens) && !has_1m_suffix(qualified) {
        format!("{qualified}{CONTEXT_1M_SUFFIX}")
    } else {
        qualified.to_string()
    }
}

fn should_advertise_1m(model_id: &str, upstream_tokens: Option<u64>) -> bool {
    inferred_max_input_tokens(model_id, upstream_tokens)
        .is_some_and(|tokens| tokens >= CONTEXT_TOKENS_1M)
}

fn looks_like_claude_1m(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    if has_1m_suffix(&id) {
        return true;
    }
    id.contains("claude-opus")
        || id.contains("claude-sonnet-5")
        || id.contains("claude-sonnet-4-6")
        || id.contains("claude-sonnet-4.6")
        || id.contains("claude-fable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_1m_suffix__stripped() {
        assert_eq!(
            strip_1m_suffix("llmgw/bedrock/claude-opus-5[1m]"),
            "llmgw/bedrock/claude-opus-5"
        );
        assert_eq!(
            strip_1m_suffix("llmgw/bedrock/claude-opus-5[1M]"),
            "llmgw/bedrock/claude-opus-5"
        );
    }

    #[test]
    fn id_without_suffix__unchanged() {
        assert_eq!(
            strip_1m_suffix("llama_swap/meta/muse-glimmer"),
            "llama_swap/meta/muse-glimmer"
        );
        assert!(!has_1m_suffix("llama_swap/meta/muse-glimmer"));
    }

    #[test]
    fn claude_opus_family__is_1m() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/bedrock/anthropic.claude-opus-5", None),
            Some(CONTEXT_TOKENS_1M)
        );
        assert_eq!(
            advertised_model_id("llmgw/bedrock/anthropic.claude-opus-5", None),
            "llmgw/bedrock/anthropic.claude-opus-5[1m]"
        );
    }

    #[test]
    fn upstream_tokens__take_priority_over_name_heuristics() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/bedrock/anthropic.claude-opus-5", Some(1_000_000)),
            Some(1_000_000)
        );
        assert_eq!(
            advertised_model_id("llmgw/bedrock/anthropic.claude-opus-5", Some(1_000_000),),
            "llmgw/bedrock/anthropic.claude-opus-5[1m]"
        );
    }

    #[test]
    fn upstream_200k__does_not_advertise_1m_even_for_opus_name() {
        assert_eq!(
            advertised_model_id("llmgw/bedrock/anthropic.claude-opus-5", Some(200_000),),
            "llmgw/bedrock/anthropic.claude-opus-5"
        );
    }

    #[test]
    fn claude_sonnet_5_family__is_1m() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/bedrock/anthropic.claude-sonnet-5", None),
            Some(CONTEXT_TOKENS_1M)
        );
    }

    #[test]
    fn already_suffixed_id__not_double_suffixed() {
        assert_eq!(
            advertised_model_id("llmgw/bedrock/claude-opus-5[1m]", Some(1_000_000)),
            "llmgw/bedrock/claude-opus-5[1m]"
        );
    }

    #[test]
    fn local_llama_id__no_inferred_window() {
        assert_eq!(
            inferred_max_input_tokens("llama_swap/meta/muse-glimmer", None),
            None
        );
        assert_eq!(
            advertised_model_id("llama_swap/meta/muse-glimmer", None),
            "llama_swap/meta/muse-glimmer"
        );
    }

    #[test]
    fn claude_haiku_id__no_inferred_window() {
        assert_eq!(
            inferred_max_input_tokens("llmgw/anthropic.claude-haiku-4-5", None),
            None
        );
    }
}

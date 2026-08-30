//! `OpenAI` `finish_reason` -> Anthropic `stop_reason`.
//!
//! `stop_sequence` is unreachable: the `OpenAI` wire format never reports *which*
//! stop string matched, so the translated message always carries
//! `"stop_sequence": null`. Faking it would be worse than omitting it.
//!
//! The map alone is not sufficient, which is why [`reconcile_tool_use`] exists:
//! Anthropic guarantees that a message containing a `tool_use` block reports
//! `stop_reason: "tool_use"`, and its SDKs drive the agent loop off exactly
//! that value rather than off the block list. Plenty of `OpenAI`-compatible
//! upstreams (llama.cpp, vLLM in some configs, several corporate gateways and
//! Bedrock behind a proxy) still send `finish_reason: "stop"` alongside
//! `tool_calls`, so translating the field in isolation would hand the client a
//! message whose blocks say "run this tool" and whose stop reason says "I am
//! done" — and the loop would end without the tool ever being called.

/// Maps an `OpenAI` `finish_reason` onto the closest Anthropic `stop_reason`.
/// An absent or unknown reason is reported as `end_turn` — the only value that
/// cannot mislead a client into retrying or truncating.
#[must_use]
pub fn map_stop_reason(finish_reason: Option<&str>) -> &'static str {
    match finish_reason.unwrap_or_default() {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

/// Upgrades a mapped stop reason to `tool_use` when the translated message
/// actually carries a `tool_use` block but the upstream did not say so.
///
/// Only `end_turn` is upgraded. `max_tokens` outranks it — a tool call cut off
/// by the token cap is truncated, and Anthropic reports that as `max_tokens` —
/// and `tool_use` is already correct.
#[must_use]
pub fn reconcile_tool_use(mapped: &'static str, saw_tool_use: bool) -> &'static str {
    if saw_tool_use && mapped == "end_turn" {
        return "tool_use";
    }
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reason_maps_stop_length_and_tool_calls() {
        assert_eq!(map_stop_reason(Some("stop")), "end_turn");
        assert_eq!(map_stop_reason(Some("length")), "max_tokens");
        assert_eq!(map_stop_reason(Some("tool_calls")), "tool_use");
        assert_eq!(map_stop_reason(Some("function_call")), "tool_use");
    }

    #[test]
    fn content_filter_reports_end_turn() {
        assert_eq!(map_stop_reason(Some("content_filter")), "end_turn");
    }

    #[test]
    fn tool_use_blocks_upgrade_a_stop_finish_reason() {
        assert_eq!(
            reconcile_tool_use(map_stop_reason(Some("stop")), true),
            "tool_use"
        );
        assert_eq!(reconcile_tool_use(map_stop_reason(None), true), "tool_use");
    }

    #[test]
    fn max_tokens_outranks_a_tool_use_block() {
        assert_eq!(
            reconcile_tool_use(map_stop_reason(Some("length")), true),
            "max_tokens"
        );
    }

    #[test]
    fn reconcile_is_a_noop_without_tool_use_blocks() {
        assert_eq!(reconcile_tool_use("end_turn", false), "end_turn");
        assert_eq!(reconcile_tool_use("max_tokens", false), "max_tokens");
        assert_eq!(reconcile_tool_use("tool_use", true), "tool_use");
    }

    #[test]
    fn unknown_finish_reason_defaults_to_end_turn() {
        assert_eq!(map_stop_reason(None), "end_turn");
        assert_eq!(map_stop_reason(Some("something_new")), "end_turn");
    }
}

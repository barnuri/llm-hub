//! Tool-name truncation and restoration.
//!
//! `OpenAI` (and Bedrock Converse behind it) reject a function name longer than
//! 64 bytes with a 400. MCP servers routinely produce longer ones — a tool
//! reached through two hops is named `mcp__<server>__<tool>` — so a client that
//! works fine against Anthropic fails against an `OpenAI`-compatible upstream
//! for a reason that has nothing to do with the conversation. This module
//! rewrites those names on the way out and puts the originals back on the way
//! in, so neither side has to know.
//!
//! Like everything under [`crate::services::transforms`], this runs on the
//! **`OpenAI`-shaped** body: by the time it sees a request the Anthropic
//! `tools[]` are already `function` tools and Anthropic `tool_use` blocks are
//! already `tool_calls`, so one implementation covers both routes.
//!
//! **The alias is deterministic and carries no state.** That is what keeps a
//! multi-turn conversation consistent across requests, and what makes a forced
//! `tool_choice` keep pointing at the same alias the `tools` array now carries,
//! with no bookkeeping at all. The Go original this is ported from spends a
//! page of package doc justifying a process-global map keyed by request id, and
//! the leaks and cross-request clobbering that come with it. Rust gives the
//! property away: a [`NameMap`] is owned by the request's transform plan, moved
//! into the response transform, moved into the response stream, and dropped
//! when the stream ends. There is no registry to leak, and a future reader
//! should not reintroduce one.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::consts::{MAX_TOOL_NAME_LEN, TOOL_NAME_HASH_LEN, TOOL_NAME_PREFIX_LEN};
use crate::utils::hex::to_hex;

/// Alias -> original tool name, for one request only.
///
/// Empty in the common case, which is what keeps the untouched path free of any
/// response-side work.
#[derive(Debug, Default, Clone)]
pub struct NameMap {
    entries: HashMap<String, String>,
}

impl NameMap {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many distinct names were aliased, for the `x-llm-hub-transforms`
    /// diagnostic header.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn insert(&mut self, alias: String, original: String) {
        self.entries.insert(alias, original);
    }

    /// The original behind an alias, or the input unchanged when it is not one.
    /// Used where a name is already isolated as a field — the Anthropic stream
    /// translator emitting `content_block_start`.
    #[must_use]
    pub fn restore_name<'a>(&'a self, alias: &'a str) -> &'a str {
        self.entries.get(alias).map_or(alias, String::as_str)
    }

    /// Puts the originals back in a JSON document, matching the alias **with
    /// its surrounding quotes** so a partial substring inside a larger value is
    /// never clobbered. Blunt on purpose: one rule covers `tool_calls[]
    /// .function.name`, the Anthropic `tool_use.name`, and any echo of the name
    /// elsewhere in the payload.
    ///
    /// Borrowed back when nothing matched, so an empty map costs nothing.
    #[must_use]
    pub fn restore_text<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for (alias, original) in &self.entries {
            let needle = format!("\"{alias}\"");
            if !out.contains(needle.as_str()) {
                continue;
            }
            out = Cow::Owned(out.replace(needle.as_str(), &format!("\"{original}\"")));
        }
        out
    }

    /// [`Self::restore_text`] over raw bytes. Non-UTF-8 input is passed through
    /// untouched rather than lossily rebuilt — a body the hub cannot read is a
    /// body it has no business rewriting.
    #[must_use]
    pub fn restore_bytes(&self, bytes: Vec<u8>) -> Vec<u8> {
        if self.entries.is_empty() {
            return bytes;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            tracing::debug!("skipping tool-name restore on a non-utf8 body");
            return bytes;
        };
        match self.restore_text(text) {
            Cow::Borrowed(_) => bytes,
            Cow::Owned(restored) => restored.into_bytes(),
        }
    }
}

/// Deterministic 64-byte-safe alias for an over-long tool name.
///
/// Names at or below [`MAX_TOOL_NAME_LEN`] are returned unchanged. Longer ones
/// become `<prefix>_<sha256[..8]>`, where the prefix is cut at
/// [`TOOL_NAME_PREFIX_LEN`] bytes and walked back to the nearest char boundary.
///
/// The boundary walk is not cosmetic: a split rune re-encodes as U+FFFD on the
/// next serialize, so the alias would stop byte-matching its own mapping key
/// and restoration would silently fail.
#[must_use]
pub fn short_name(name: &str) -> Cow<'_, str> {
    if name.len() <= MAX_TOOL_NAME_LEN {
        return Cow::Borrowed(name);
    }
    let digest = to_hex(&Sha256::digest(name.as_bytes()));
    let hash = digest.get(..TOOL_NAME_HASH_LEN).unwrap_or(digest.as_str());
    let mut cut = TOOL_NAME_PREFIX_LEN;
    // `name` is longer than the cut, and 0 is always a boundary, so this
    // terminates without ever indexing out of range.
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!("{}_{hash}", &name[..cut]))
}

/// Rewrites every over-long tool name in an `OpenAI`-shaped request body and
/// returns the mapping needed to undo it.
///
/// Takes the body's object rather than the `Value` because the only caller
/// ([`crate::services::transforms::apply_request`]) has already established
/// that the body is an object.
///
/// Fail-open at every level: a missing key or a wrong-typed value is skipped,
/// the rest is still processed, and a body with no long names is left
/// byte-identical.
#[must_use]
pub fn truncate_in_body(body: &mut Map<String, Value>) -> NameMap {
    let mut names = NameMap::default();

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            rewrite_function_name(tool, &mut names);
        }
    }

    // A forced tool choice names the same tool the `tools` array carries; the
    // alias is deterministic, so the two stay in sync for free. String forms
    // (`auto`, `none`, `required`) are not objects and pass through.
    if let Some(choice) = body.get_mut("tool_choice") {
        rewrite_function_name(choice, &mut names);
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            for call in calls {
                rewrite_function_name(call, &mut names);
            }
        }
    }

    names
}

/// Rewrites `entry.function.name`, falling back to a bare `entry.name` when the
/// entry carries no `function` wrapper at all (both shapes appear in the wild
/// for `tools[]` and for `tool_choice`).
fn rewrite_function_name(entry: &mut Value, names: &mut NameMap) {
    if let Some(function) = entry.get_mut("function")
        && function.get("name").is_some()
    {
        rewrite_name(function, names);
        return;
    }
    rewrite_name(entry, names);
}

fn rewrite_name(container: &mut Value, names: &mut NameMap) {
    let Some(Value::String(name)) = container.get_mut("name") else {
        return;
    };
    if name.len() <= MAX_TOOL_NAME_LEN {
        return;
    }
    let alias = short_name(name).into_owned();
    names.insert(alias.clone(), std::mem::replace(name, alias));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape that motivates the whole module: an MCP tool reached through a
    /// server prefix, 77 bytes.
    const MCP: &str =
        "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";

    fn truncate(body: &mut Value) -> NameMap {
        truncate_in_body(body.as_object_mut().expect("object body"))
    }

    #[test]
    fn short_name_is_identity_at_or_below_64_bytes() {
        for name in ["search", "", &"a".repeat(MAX_TOOL_NAME_LEN)] {
            assert_eq!(short_name(name), Cow::Borrowed(name));
        }
    }

    #[test]
    fn short_name_produces_exactly_64_bytes_for_ascii() {
        let alias = short_name(MCP).into_owned();
        assert_eq!(alias.len(), MAX_TOOL_NAME_LEN);
        assert!(
            alias.starts_with("mcp__github_enterprise__very_long"),
            "{alias}"
        );
        let (prefix, hash) = alias.rsplit_once('_').unwrap();
        assert_eq!(prefix.len(), TOOL_NAME_PREFIX_LEN);
        assert_eq!(hash.len(), TOOL_NAME_HASH_LEN);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");
    }

    #[test]
    fn short_name_is_deterministic_across_calls() {
        assert_eq!(short_name(MCP), short_name(MCP));
        // And across a round trip through the body rewriter, which is what
        // keeps a multi-turn conversation consistent with no stored state.
        let mut body = json!({"tools": [{"function": {"name": MCP}}]});
        truncate(&mut body);
        assert_eq!(body["tools"][0]["function"]["name"], *short_name(MCP));
    }

    #[test]
    fn distinct_long_names_sharing_a_prefix_get_distinct_aliases() {
        let stem = "mcp__server__".to_string() + &"x".repeat(60);
        let a = format!("{stem}_alpha");
        let b = format!("{stem}_beta");
        // The 55-byte prefixes are identical: only the digest separates them.
        assert_eq!(&a[..TOOL_NAME_PREFIX_LEN], &b[..TOOL_NAME_PREFIX_LEN]);
        assert_ne!(short_name(&a), short_name(&b));
    }

    #[test]
    fn short_name_respects_char_boundaries_and_stays_within_the_limit() {
        // Every char is 3 bytes, so the 55-byte cut lands mid-rune.
        let name = "あ".repeat(30);
        assert!(name.len() > MAX_TOOL_NAME_LEN);
        let alias = short_name(&name).into_owned();
        assert!(alias.len() <= MAX_TOOL_NAME_LEN, "{}", alias.len());
        assert!(!alias.contains('\u{fffd}'), "{alias}");
        // Round-tripping through JSON must not change a byte, or the alias
        // would stop matching its own mapping key.
        let encoded = serde_json::to_string(&alias).unwrap();
        let decoded: String = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, alias);
    }

    #[test]
    fn truncate_in_body_rewrites_tools_function_name() {
        let mut body = json!({"tools": [
            {"type": "function", "function": {"name": MCP, "parameters": {}}},
            {"type": "function", "function": {"name": "short"}},
        ]});
        let names = truncate(&mut body);
        assert_eq!(names.len(), 1);
        let alias = body["tools"][0]["function"]["name"].as_str().unwrap();
        assert_eq!(alias.len(), MAX_TOOL_NAME_LEN);
        assert_eq!(names.restore_name(alias), MCP);
        assert_eq!(body["tools"][1]["function"]["name"], "short");
    }

    #[test]
    fn truncate_in_body_rewrites_a_bare_tools_name_fallback() {
        let mut body = json!({"tools": [{"name": MCP, "input_schema": {}}]});
        let names = truncate(&mut body);
        assert_eq!(names.len(), 1);
        assert_eq!(
            names.restore_name(body["tools"][0]["name"].as_str().unwrap()),
            MCP
        );
    }

    #[test]
    fn truncate_in_body_rewrites_message_tool_calls() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [
                {"id": "call_1", "function": {"name": MCP, "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "done"},
        ]});
        let names = truncate(&mut body);
        let alias = body["messages"][1]["tool_calls"][0]["function"]["name"]
            .as_str()
            .unwrap();
        assert_eq!(names.restore_name(alias), MCP);
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn truncate_in_body_rewrites_both_tool_choice_object_forms() {
        let wrapped = json!({"type": "function", "function": {"name": MCP}});
        let bare = json!({"type": "function", "name": MCP});
        for choice in [wrapped, bare] {
            let mut body = json!({"tools": [{"function": {"name": MCP}}], "tool_choice": choice});
            let names = truncate(&mut body);
            // One entry, because the alias is derived, not allocated.
            assert_eq!(names.len(), 1);
            let chosen = body["tool_choice"]["function"]["name"]
                .as_str()
                .or_else(|| body["tool_choice"]["name"].as_str())
                .unwrap();
            assert_eq!(chosen, body["tools"][0]["function"]["name"]);
            assert_eq!(names.restore_name(chosen), MCP);
        }
    }

    #[test]
    fn string_tool_choice_passes_through() {
        for choice in ["auto", "none", "required"] {
            let mut body = json!({"tools": [], "tool_choice": choice});
            let names = truncate(&mut body);
            assert!(names.is_empty());
            assert_eq!(body["tool_choice"], choice);
        }
    }

    #[test]
    fn noop_when_all_names_are_short() {
        let mut body = json!({
            "model": "p/m",
            "tools": [{"type": "function", "function": {"name": "search"}}],
            "tool_choice": {"type": "function", "function": {"name": "search"}},
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": "search", "arguments": "{}"}}
            ]}],
        });
        let before = body.clone();
        let names = truncate(&mut body);
        assert!(names.is_empty());
        assert_eq!(body, before);
    }

    #[test]
    fn malformed_entries_are_skipped_without_disturbing_the_rest() {
        let mut body = json!({
            "tools": ["not an object", {"function": {"name": 7}}, {"function": {"name": MCP}}],
            "tool_choice": 42,
            "messages": [{"role": "assistant", "tool_calls": "nope"}],
        });
        let names = truncate(&mut body);
        assert_eq!(names.len(), 1);
        assert_eq!(body["tools"][0], "not an object");
        assert_eq!(body["tools"][1]["function"]["name"], 7);
        assert_eq!(body["tool_choice"], 42);
    }

    #[test]
    fn an_already_truncated_alias_is_idempotent() {
        let alias = short_name(MCP).into_owned();
        // The client echoes the alias back on the next turn while the tools
        // array still declares the original: both must resolve to one name.
        let mut body = json!({
            "tools": [{"function": {"name": MCP}}],
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": alias, "arguments": "{}"}}
            ]}],
        });
        let names = truncate(&mut body);
        assert_eq!(names.len(), 1);
        assert_eq!(body["tools"][0]["function"]["name"], alias);
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["name"],
            alias
        );
    }

    #[test]
    fn restore_replaces_the_quoted_alias_only() {
        let mut body = json!({"tools": [{"function": {"name": MCP}}]});
        let names = truncate(&mut body);
        let alias = short_name(MCP).into_owned();
        let response = format!(
            "{{\"name\":\"{alias}\",\"note\":\"prefix {alias} suffix\",\"other\":\"{alias}x\"}}"
        );
        let restored = names.restore_text(&response);
        assert!(
            restored.contains(&format!("\"name\":\"{MCP}\"")),
            "{restored}"
        );
        // A partial substring inside a larger value keeps the alias.
        assert!(
            restored.contains(&format!("prefix {alias} suffix")),
            "{restored}"
        );
        assert!(restored.contains(&format!("\"{alias}x\"")), "{restored}");
    }

    #[test]
    fn an_empty_map_restores_byte_identically() {
        let names = NameMap::default();
        let payload = br#"{"name":"search"}"#.to_vec();
        assert_eq!(names.restore_bytes(payload.clone()), payload);
        assert!(matches!(
            names.restore_text("{\"name\":\"search\"}"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn restore_bytes_leaves_a_non_utf8_body_alone() {
        let mut body = json!({"tools": [{"function": {"name": MCP}}]});
        let names = truncate(&mut body);
        let payload = vec![0xff, 0xfe, 0x00];
        assert_eq!(names.restore_bytes(payload.clone()), payload);
    }
}

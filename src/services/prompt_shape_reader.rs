use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use serde_json::Value;

use crate::schemas::prompt_shape::PromptShape;

impl PromptShape {
    const DEFERRED_LISTING: &str = "The following deferred tools are now available via ToolSearch";
    const SKILLS_LISTING: &str = "The following skills are available";
    const CLAUDE_MD_OPEN: &str = "Contents of ";
    const CLAUDE_MD_FILE: &str = "CLAUDE.md (";
    const WORKING_DIRECTORY: &str = "Primary working directory:";
    const MCP_TOOL_PREFIX: &str = "mcp__";
    const SYSTEM_REMINDER_CLOSE: &str = "</system-reminder>";
    const SKILL_BODY_OPEN: &str = "Base directory for this skill:";
    const TOOL_FAMILY_OTHER: &str = "other";
    const NAMED_TOOLS: [&str; 4] = ["Read", "Bash", "Agent", "Skill"];
    const TOP_TOOL_FAMILIES: usize = 12;
    const HASH_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const HASH_PRIME: u64 = 0x0000_0100_0000_01b3;

    /// Reads the counts and sizes of one raw Anthropic `/v1/messages` body in one pass. Text is only
    /// measured, hashed and searched, never copied. `None` when the body is not a JSON object.
    #[must_use]
    pub fn from_body(raw: &[u8]) -> Option<PromptShape> {
        let body: Value = serde_json::from_slice(raw).ok()?;
        let fields = body.as_object()?;
        let mut shape = PromptShape::default();
        let mut stream_hash = Self::HASH_OFFSET;
        for text in fields.get("system").into_iter().flat_map(Self::text_parts) {
            shape.system_chars += Self::char_count(text);
            shape.note_text(text);
            Self::feed_hash(&mut stream_hash, text.as_bytes());
        }
        Self::feed_hash(&mut stream_hash, &[0]);
        if let Some(messages) = fields.get("messages").and_then(Value::as_array) {
            shape.messages = Self::to_u64(messages.len());
            for message in messages {
                let Some(content) = message.get("content") else {
                    continue;
                };
                for text in Self::text_parts(content) {
                    shape.note_text(text);
                }
                shape.image_count += Self::image_count(content);
            }
            shape.turn_start = messages.last().is_some_and(|message| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && message
                        .get("content")
                        .is_some_and(|content| Self::text_parts(content).next().is_some())
            });
            (shape.tool_result_bytes, shape.skill_body_chars) = Self::message_usage(messages);
        }
        let mut tool_names: Vec<&str> = Vec::new();
        if let Some(tools) = fields.get("tools").and_then(Value::as_array) {
            shape.tool_count = Self::to_u64(tools.len());
            for tool in tools {
                let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
                tool_names.push(name);
                let bytes = Self::serialized_len(tool);
                shape.tool_bytes += bytes;
                if name.starts_with(Self::MCP_TOOL_PREFIX) {
                    shape.mcp_tool_count += 1;
                    shape.mcp_tool_bytes += bytes;
                }
            }
        }
        tool_names.sort_unstable();
        for name in tool_names {
            Self::feed_hash(&mut stream_hash, name.as_bytes());
            Self::feed_hash(&mut stream_hash, &[0]);
        }
        shape.stream_key = Some(format!("{stream_hash:016x}"));
        Some(shape)
    }

    /// Sizes each listing and CLAUDE.md file from its marker to the next marker or block end.
    /// The first listing and working directory win. CLAUDE.md segments are summed.
    fn note_text(&mut self, text: &str) {
        let markers = Self::marker_offsets(text);
        for &(start, marker) in &markers {
            let end = markers
                .iter()
                .map(|&(offset, _)| offset)
                .find(|&offset| offset > start)
                .unwrap_or(text.len());
            let segment = &text[start..end];
            let chars = Self::char_count(segment);
            match marker {
                Self::DEFERRED_LISTING if self.deferred_listing_chars.is_none() => {
                    self.deferred_listing_chars = Some(chars);
                }
                Self::SKILLS_LISTING if self.skills_listing_chars.is_none() => {
                    self.skills_listing_chars = Some(chars);
                }
                Self::CLAUDE_MD_OPEN if segment.contains(Self::CLAUDE_MD_FILE) => {
                    *self.claude_md_chars.get_or_insert(0) += chars;
                }
                _ => {}
            }
        }
        if self.project.is_none()
            && let Some(project) = Self::project_basename(text)
        {
            self.project = Some(project);
        }
    }

    /// Every marker occurrence in `text` as (byte offset, marker), in offset order.
    fn marker_offsets(text: &str) -> Vec<(usize, &'static str)> {
        let markers = [
            Self::DEFERRED_LISTING,
            Self::SKILLS_LISTING,
            Self::CLAUDE_MD_OPEN,
            Self::SYSTEM_REMINDER_CLOSE,
        ];
        let mut found: Vec<(usize, &'static str)> = markers
            .into_iter()
            .flat_map(|marker| text.match_indices(marker).map(move |(at, _)| (at, marker)))
            .collect();
        found.sort_unstable();
        found
    }

    /// Folds `bytes` into a 64-bit FNV-1a hash. The hash is stable across Rust releases, unlike `DefaultHasher`.
    fn feed_hash(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(Self::HASH_PRIME);
        }
    }

    /// The text of each text block of a `system` or message `content` value. A bare string is one block.
    fn text_parts(content: &Value) -> impl Iterator<Item = &str> + '_ {
        let bare = content.as_str();
        let blocks = content
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Self::text_of_block);
        bare.into_iter().chain(blocks)
    }

    fn text_of_block(block: &Value) -> Option<&str> {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return None;
        }
        block.get("text").and_then(Value::as_str)
    }

    /// Image blocks in one message `content`, counting the ones nested in tool results.
    fn image_count(content: &Value) -> u64 {
        let Some(blocks) = content.as_array() else {
            return 0;
        };
        blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("image") => 1,
                Some("tool_result") => block.get("content").map_or(0, Self::image_count),
                _ => 0,
            })
            .sum()
    }

    /// The last path segment of the directory a text block states after `Primary working directory:`.
    fn project_basename(text: &str) -> Option<String> {
        let (_, rest) = text.split_once(Self::WORKING_DIRECTORY)?;
        Self::basename(rest.lines().next()?)
    }

    /// The last segment of a path, ignoring trailing separators. `None` when nothing is left.
    fn basename(path: &str) -> Option<String> {
        let name = path
            .trim()
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()?
            .trim();
        (!name.is_empty()).then(|| name.to_owned())
    }

    /// Tool-result text bytes by tool family and skill-body characters by skill name, over every message.
    fn message_usage(messages: &[Value]) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
        let mut tool_names: HashMap<&str, &str> = HashMap::new();
        for block in messages
            .iter()
            .filter_map(|message| message.get("content"))
            .flat_map(Self::blocks)
        {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
                tool_names.insert(id, name);
            }
        }
        let mut families: BTreeMap<String, u64> = BTreeMap::new();
        let mut skills: BTreeMap<String, u64> = BTreeMap::new();
        for message in messages {
            let Some(content) = message.get("content") else {
                continue;
            };
            if message.get("role").and_then(Value::as_str) == Some("user") {
                for text in Self::text_parts(content) {
                    if let Some(name) = Self::skill_name(text) {
                        *skills.entry(name).or_default() += Self::char_count(text);
                    }
                }
            }
            for block in Self::blocks(content) {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                let name = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .and_then(|id| tool_names.get(id))
                    .copied()
                    .unwrap_or_default();
                let bytes = Self::result_text_bytes(block.get("content"));
                *families.entry(Self::tool_family(name)).or_default() += bytes;
            }
        }
        (Self::top_families(families), skills)
    }

    /// The family a tool name counts under: `mcp__<server>` for an MCP tool, the named tool, or `other`.
    fn tool_family(name: &str) -> String {
        let server = name
            .strip_prefix(Self::MCP_TOOL_PREFIX)
            .and_then(|rest| rest.split("__").next())
            .filter(|server| !server.is_empty());
        if let Some(server) = server {
            return format!("{}{server}", Self::MCP_TOOL_PREFIX);
        }
        if Self::NAMED_TOOLS.contains(&name) {
            name.to_owned()
        } else {
            Self::TOOL_FAMILY_OTHER.to_owned()
        }
    }

    /// Keeps the largest named families, ties by name, and sums the rest into `other`.
    fn top_families(families: BTreeMap<String, u64>) -> BTreeMap<String, u64> {
        let other = families.get(Self::TOOL_FAMILY_OTHER).copied();
        let mut named: Vec<(String, u64)> = families
            .into_iter()
            .filter(|(family, _)| family != Self::TOOL_FAMILY_OTHER)
            .collect();
        named.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let mut kept = BTreeMap::new();
        let mut rest = other.unwrap_or(0);
        let mut overflowed = false;
        for (index, (family, bytes)) in named.into_iter().enumerate() {
            if index < Self::TOP_TOOL_FAMILIES {
                kept.insert(family, bytes);
            } else {
                rest += bytes;
                overflowed = true;
            }
        }
        if other.is_some() || overflowed {
            kept.insert(Self::TOOL_FAMILY_OTHER.to_owned(), rest);
        }
        kept
    }

    /// Text bytes of one tool result: a bare string, or the text blocks of a block list. Images count nothing.
    fn result_text_bytes(content: Option<&Value>) -> u64 {
        match content {
            Some(Value::String(text)) => Self::to_u64(text.len()),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(Self::text_of_block)
                .map(|text| Self::to_u64(text.len()))
                .sum(),
            _ => 0,
        }
    }

    /// The skill directory name a text block states after `Base directory for this skill:`.
    fn skill_name(text: &str) -> Option<String> {
        let rest = text.trim_start().strip_prefix(Self::SKILL_BODY_OPEN)?;
        Self::basename(rest.lines().next()?)
    }

    /// The blocks of a message `content` value. A bare string has none.
    fn blocks(content: &Value) -> impl Iterator<Item = &Value> + '_ {
        content.as_array().into_iter().flatten()
    }

    /// Serialized size of a JSON value in bytes, counted without keeping the output.
    fn serialized_len(value: &Value) -> u64 {
        let mut counter = ByteCounter::default();
        match serde_json::to_writer(&mut counter, value) {
            Ok(()) => counter.0,
            Err(_) => 0,
        }
    }

    fn char_count(text: &str) -> u64 {
        Self::to_u64(text.chars().count())
    }

    fn to_u64(n: usize) -> u64 {
        u64::try_from(n).unwrap_or(u64::MAX)
    }
}

#[derive(Default)]
struct ByteCounter(u64);

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += PromptShape::to_u64(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).unwrap()
    }

    fn shape_of(value: &Value) -> PromptShape {
        PromptShape::from_body(&body(value)).unwrap()
    }

    #[test]
    fn system_as_blocks_sums_every_text_block() {
        let shape = shape_of(&json!({
            "system": [
                {"type": "text", "text": "abcd"},
                {"type": "image", "source": {"data": "zzzz"}},
                {"type": "text", "text": "efghij"},
            ],
            "messages": [],
        }));
        assert_eq!(shape.system_chars, 10);
    }

    #[test]
    fn system_as_string_counts_chars() {
        let shape = shape_of(&json!({"system": "héllo", "messages": []}));
        assert_eq!(shape.system_chars, 5);
    }

    #[test]
    fn mcp_tools_counted_by_prefix_only() {
        let mcp = json!({"name": "mcp__files__read", "description": "d", "input_schema": {}});
        let plain = json!({"name": "read_file", "description": "d", "input_schema": {}});
        let shape = shape_of(&json!({
            "tools": [mcp.clone(), plain.clone(), {"name": "x_mcp__y"}],
            "messages": [],
        }));
        assert_eq!(shape.tool_count, 3);
        assert_eq!(shape.mcp_tool_count, 1);
        assert_eq!(shape.mcp_tool_bytes, PromptShape::serialized_len(&mcp));
        assert_eq!(
            shape.tool_bytes,
            PromptShape::serialized_len(&mcp)
                + PromptShape::serialized_len(&plain)
                + PromptShape::serialized_len(&json!({"name": "x_mcp__y"}))
        );
    }

    #[test]
    fn marker_absent_listing_is_none() {
        let shape = shape_of(&json!({
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "hello there"}],
        }));
        assert_eq!(shape.deferred_listing_chars, None);
        assert_eq!(shape.skills_listing_chars, None);
        assert_eq!(shape.claude_md_chars, None);
        assert_eq!(shape.project, None);
    }

    #[test]
    fn listings_and_claude_md_sized_from_marker_blocks() {
        let deferred = format!("{}: alpha, beta", PromptShape::DEFERRED_LISTING);
        let skills = format!(
            "{} for use with the Skill tool: gamma",
            PromptShape::SKILLS_LISTING
        );
        let claude_one = "Contents of /work/one/CLAUDE.md (project instructions): one";
        let claude_two = "Contents of /work/two/CLAUDE.md (user instructions): two!!";
        let shape = shape_of(&json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": deferred},
                {"type": "text", "text": skills},
                {"type": "text", "text": claude_one},
                {"type": "text", "text": claude_two},
            ]}],
        }));
        assert_eq!(
            shape.deferred_listing_chars,
            Some(PromptShape::to_u64(deferred.chars().count()))
        );
        assert_eq!(
            shape.skills_listing_chars,
            Some(PromptShape::to_u64(skills.chars().count()))
        );
        assert_eq!(
            shape.claude_md_chars,
            Some(PromptShape::to_u64(
                claude_one.chars().count() + claude_two.chars().count()
            ))
        );
    }

    #[test]
    fn project_path_keeps_basename_only() {
        let shape = shape_of(&json!({
            "system": [{"type": "text", "text": "Primary working directory: /home/dev/sample-project/\nIs a git repository: true"}],
            "messages": [],
        }));
        assert_eq!(shape.project.as_deref(), Some("sample-project"));
        assert!(!format!("{shape:?}").contains("/home/dev"));
    }

    #[test]
    fn last_message_tool_result_not_a_turn_start() {
        let shape = shape_of(&json!({
            "messages": [
                {"role": "user", "content": "run it"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
            ],
        }));
        assert_eq!(shape.messages, 3);
        assert!(!shape.turn_start);
    }

    #[test]
    fn last_message_with_text_is_a_turn_start() {
        let shape = shape_of(&json!({
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": "next question"}]},
            ],
        }));
        assert!(shape.turn_start);
    }

    #[test]
    fn image_blocks_counted_including_tool_results() {
        let shape = shape_of(&json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"data": "aaaa"}},
                    {"type": "text", "text": "look"},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "image", "source": {"data": "bbbb"}},
                    ]},
                ]},
            ],
        }));
        assert_eq!(shape.image_count, 2);
    }

    #[test]
    fn invalid_json_none() {
        assert_eq!(PromptShape::from_body(b"{not json"), None);
        assert_eq!(PromptShape::from_body(b"[1, 2, 3]"), None);
    }

    #[test]
    fn prompt_text_never_stored_in_shape() {
        let secret = "SECRET-PROMPT-TEXT-0451";
        let shape = shape_of(&json!({
            "system": secret,
            "messages": [{"role": "user", "content": secret}],
            "tools": [{"name": "mcp__x__y", "description": secret}],
        }));
        assert_eq!(shape.system_chars, PromptShape::to_u64(secret.len()));
        assert!(!format!("{shape:?}").contains(secret));
    }

    #[test]
    fn stream_key_follows_system_and_tool_names_only() {
        let main = json!({
            "system": "You are the main agent.",
            "tools": [{"name": "bash"}, {"name": "read_file"}],
            "messages": [{"role": "user", "content": "first"}],
        });
        let same_setup_later = json!({
            "system": "You are the main agent.",
            "tools": [{"name": "read_file", "description": "changed"}, {"name": "bash"}],
            "messages": [{"role": "user", "content": "later"}, {"role": "assistant", "content": "ok"}],
        });
        let subagent = json!({
            "system": "You are a planner.",
            "tools": [{"name": "bash"}, {"name": "read_file"}],
            "messages": [{"role": "user", "content": "first"}],
        });
        let other_tools = json!({
            "system": "You are the main agent.",
            "tools": [{"name": "bash"}],
            "messages": [{"role": "user", "content": "first"}],
        });
        let key = |value: &Value| shape_of(value).stream_key;
        assert!(key(&main).is_some_and(|key| key.len() == 16));
        assert_eq!(key(&main), key(&same_setup_later));
        assert_ne!(key(&main), key(&subagent));
        assert_ne!(key(&main), key(&other_tools));
    }

    #[test]
    fn one_block_with_both_listings_measures_each_to_the_next_marker() {
        let deferred = format!("{}: alpha, beta", PromptShape::DEFERRED_LISTING);
        let skills = format!("{}: gamma, delta, epsilon", PromptShape::SKILLS_LISTING);
        let text = format!("{deferred}\n{skills}\n</system-reminder>\nunrelated tail text");
        let shape = shape_of(&json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": text}]}],
        }));
        let deferred_chars = PromptShape::to_u64(deferred.chars().count() + 1);
        let skills_chars = PromptShape::to_u64(skills.chars().count() + 1);
        assert_eq!(shape.deferred_listing_chars, Some(deferred_chars));
        assert_eq!(shape.skills_listing_chars, Some(skills_chars));
        assert_ne!(deferred_chars, skills_chars);
    }

    #[test]
    fn claude_md_files_in_one_block_each_measured_to_the_next_file() {
        let first = "Contents of /work/one/CLAUDE.md (project instructions): one";
        let second = "Contents of /work/two/CLAUDE.md (user instructions): two!!";
        let text = format!("{first}\n{second}");
        let shape = shape_of(&json!({
            "system": [{"type": "text", "text": text}],
            "messages": [],
        }));
        assert_eq!(
            shape.claude_md_chars,
            Some(PromptShape::to_u64(
                first.chars().count() + 1 + second.chars().count()
            ))
        );
    }

    #[test]
    fn tool_results_summed_by_family_through_tool_use_ids() {
        let shape = shape_of(&json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "a", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "b", "name": "mcp__files__read", "input": {}},
                    {"type": "tool_use", "id": "c", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "d", "name": "Agent", "input": {}},
                    {"type": "tool_use", "id": "e", "name": "Skill", "input": {}},
                    {"type": "tool_use", "id": "f", "name": "WebFetch", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "content": "abcd"},
                    {"type": "tool_result", "tool_use_id": "b", "content": [
                        {"type": "text", "text": "xyz"},
                        {"type": "image", "source": {"data": "zzzz"}},
                    ]},
                    {"type": "tool_result", "tool_use_id": "c", "content": "é"},
                    {"type": "tool_result", "tool_use_id": "d", "content": ""},
                    {"type": "tool_result", "tool_use_id": "e", "content": "skillbody"},
                    {"type": "tool_result", "tool_use_id": "f", "content": "other-result"},
                    {"type": "tool_result", "tool_use_id": "unknown", "content": "q"},
                ]},
            ],
        }));
        assert_eq!(
            shape.tool_result_bytes,
            BTreeMap::from([
                ("Agent".to_owned(), 0),
                ("Bash".to_owned(), 2),
                ("Read".to_owned(), 4),
                ("Skill".to_owned(), 9),
                ("mcp__files".to_owned(), 3),
                ("other".to_owned(), 13),
            ])
        );
    }

    #[test]
    fn tool_family_names_mcp_by_server_and_the_rest_by_name() {
        assert_eq!(PromptShape::tool_family("mcp__files__read"), "mcp__files");
        assert_eq!(PromptShape::tool_family("mcp__files"), "mcp__files");
        assert_eq!(PromptShape::tool_family("mcp__"), "other");
        assert_eq!(PromptShape::tool_family("Read"), "Read");
        assert_eq!(PromptShape::tool_family("Glob"), "other");
        assert_eq!(PromptShape::tool_family(""), "other");
    }

    #[test]
    fn top_twelve_families_named_and_the_rest_summed_into_other() {
        let mut families: BTreeMap<String, u64> = (0..14_u64)
            .map(|index| (format!("mcp__s{index:02}"), (index + 1) * 10))
            .collect();
        families.insert("other".to_owned(), 5);
        let kept = PromptShape::top_families(families);
        assert_eq!(kept.len(), 13);
        assert_eq!(kept.get("other"), Some(&35));
        assert!(!kept.contains_key("mcp__s00"));
        assert!(!kept.contains_key("mcp__s01"));
        assert_eq!(kept.get("mcp__s13"), Some(&140));
    }

    #[test]
    fn skill_bodies_counted_by_directory_name_across_user_blocks() {
        let first = "Base directory for this skill: /home/dev/skills/pdf\n\n# PDF SECRET BODY 4471";
        let second = "Base directory for this skill: /home/dev/skills/pdf/\nmore";
        let shape = shape_of(&json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": first},
                    {"type": "text", "text": second},
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Base directory for this skill: /home/dev/skills/planner"},
                ]},
                {"role": "user", "content": "not a skill: Base directory for this skill: /x/inline"},
            ],
        }));
        assert_eq!(
            shape.skill_body_chars,
            BTreeMap::from([(
                "pdf".to_owned(),
                PromptShape::char_count(first) + PromptShape::char_count(second),
            )])
        );
        assert!(!format!("{shape:?}").contains("SECRET BODY"));
    }

    #[test]
    fn skill_name_is_the_last_path_segment() {
        assert_eq!(
            PromptShape::skill_name("Base directory for this skill: /a/b/tool"),
            Some("tool".to_owned())
        );
        assert_eq!(
            PromptShape::skill_name("  Base directory for this skill: C:\\skills\\win\\"),
            Some("win".to_owned())
        );
        assert_eq!(
            PromptShape::skill_name("Base directory for this skill:"),
            None
        );
        assert_eq!(PromptShape::skill_name("plain text"), None);
    }

    #[test]
    fn usage_maps_are_empty_without_tools_or_skills() {
        let shape = shape_of(&json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert!(shape.tool_result_bytes.is_empty());
        assert!(shape.skill_body_chars.is_empty());
    }
}

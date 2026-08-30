//! The one place a response body is allowed to be rewritten.
//!
//! Every variant obeys the same buffering rule: a transform may buffer a
//! **non-SSE** body in full, but must never hold an **SSE** body beyond a
//! single frame. The two SSE variants are frame-at-a-time state machines, so
//! streaming stays streaming; [`BufferedRestore`] is the buffered one, and it
//! is only ever selected for a body that is not a stream.
//!
//! `None` (no transform at all) remains the default on every other route: it
//! is byte-for-byte passthrough with no copy.

use serde_json::Value;

use crate::consts::MAX_TRANSFORM_BUFFER_BYTES;
use crate::services::anthropic::stream::AnthropicStream;
use crate::services::sse::{SseFrames, parse_data_frame};
use crate::services::tool_names::NameMap;

const ASSISTANT: &str = "assistant";

pub enum BodyTransform {
    /// `OpenAI` SSE in, Anthropic Messages SSE out. Boxed because the state
    /// machine is far larger than anything else that will live in this enum.
    AnthropicStream(Box<AnthropicStream>),
    /// `OpenAI` SSE in, `OpenAI` SSE out, with `role:"assistant"` added to the
    /// first delta when the upstream omitted it and truncated tool names put
    /// back frame by frame.
    Sse(SseRewrite),
    /// A whole non-SSE body, held until the end and handed back with the
    /// original tool names restored.
    Buffered(BufferedRestore),
}

impl BodyTransform {
    /// Translates an `OpenAI` chat-completions stream into an Anthropic
    /// Messages stream, reporting `served_model` as the model in
    /// `message_start` and restoring `names` on every `tool_use` block.
    #[must_use]
    pub fn anthropic_stream(served_model: String, names: NameMap) -> Self {
        Self::AnthropicStream(Box::new(AnthropicStream::new(served_model, names)))
    }

    /// Rewrites an `OpenAI` stream: the first-delta `role` repair, the tool
    /// names, or both. With nothing left to do it disengages to raw
    /// passthrough for the rest of the stream.
    #[must_use]
    pub fn sse(inject_role: bool, names: NameMap) -> Self {
        Self::Sse(SseRewrite::new(inject_role, names))
    }

    /// Buffers a non-SSE body and puts the original tool names back.
    #[must_use]
    pub fn name_restore(names: NameMap) -> Self {
        Self::Buffered(BufferedRestore::new(names))
    }

    /// Feeds one upstream chunk and returns the bytes to send on. May be empty.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::AnthropicStream(stream) => stream.push(bytes),
            Self::Sse(rewrite) => rewrite.push(bytes),
            Self::Buffered(buffered) => buffered.push(bytes),
        }
    }

    /// End of upstream: whatever is needed to leave the client with a
    /// well-formed body. Idempotent.
    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::AnthropicStream(stream) => stream.finish(),
            Self::Sse(rewrite) => rewrite.finish(),
            Self::Buffered(buffered) => buffered.finish(),
        }
    }

    /// Token counts the transform saw while reading the body, for stats.
    /// `None` means "fall back to scraping the tail", which is what the
    /// untransformed path does.
    #[must_use]
    pub fn observed_usage(&self) -> Option<crate::services::stats::ScrapedUsage> {
        match self {
            Self::AnthropicStream(stream) => stream.observed_usage(),
            // Neither of these touches the usage object, so the raw upstream
            // tail is still the authoritative source.
            Self::Sse(_) | Self::Buffered(_) => None,
        }
    }
}

/// Minimal `OpenAI`-SSE rewriter, doing at most two jobs.
///
/// The first is a spec-conformance repair: the first `choices[0].delta` of a
/// stream must carry `role`, and some local servers omit it, which breaks
/// clients that build a message from the deltas alone (`LangChain`'s
/// `get_final_completion()` with a `response_format` is the canonical victim).
/// It is a single edit to a single frame.
///
/// The second is putting truncated tool names back, which — unlike the repair —
/// can be needed on any frame, since a `tool_calls[].function.name` may arrive
/// at any point in the stream. Substitution happens **per frame**, never per
/// raw chunk: an alias can straddle a TCP boundary but never a frame boundary.
///
/// With both jobs done (or never requested) the rewriter **disengages**: from
/// then on chunks are forwarded untouched, with no framing, no parsing and no
/// copy beyond the move out of the buffer. That is what keeps an always-on
/// transform honest on a long stream.
pub struct SseRewrite {
    frames: SseFrames,
    /// Still looking for the first `data:` frame.
    pending_role: bool,
    /// Alias -> original tool name; empty means nothing to restore.
    names: NameMap,
    /// Disengaged: everything from here is forwarded verbatim.
    raw: bool,
}

impl SseRewrite {
    fn new(inject_role: bool, names: NameMap) -> Self {
        Self {
            frames: SseFrames::new(),
            pending_role: inject_role,
            names,
            raw: false,
        }
    }

    /// True once no frame can still need rewriting — which, when names are in
    /// play, is never, because a tool call may appear in any frame.
    fn done(&self) -> bool {
        !self.pending_role && self.names.is_empty()
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.raw {
            return bytes.to_vec();
        }
        let frames = self.frames.push(bytes);
        let mut out = String::new();
        for frame in frames {
            out.push_str(&self.rewrite(&frame));
        }
        let mut out = out.into_bytes();
        if self.done() {
            // Nothing left to look for. Hand over whatever partial frame is
            // still buffered and stop splitting for the rest of the stream.
            out.extend_from_slice(&self.frames.take_pending());
            self.raw = true;
        }
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.raw {
            return Vec::new();
        }
        self.raw = true;
        self.frames.take_pending()
    }

    /// One frame in, one frame out: the role repair (guarded to the first data
    /// frame that parses, has `choices[0].delta`, and has no `role` already),
    /// then the name restore.
    fn rewrite(&mut self, frame: &str) -> String {
        let repaired = self.repair_role(frame);
        match self.names.restore_text(&repaired) {
            std::borrow::Cow::Borrowed(_) => repaired,
            std::borrow::Cow::Owned(restored) => restored,
        }
    }

    fn repair_role(&mut self, frame: &str) -> String {
        if !self.pending_role {
            return format!("{frame}\n\n");
        }
        let Some(payload) = parse_data_frame(frame) else {
            // A comment or bare `event:` frame is not the first *data* frame.
            return format!("{frame}\n\n");
        };
        // One chance only: if the first data frame does not qualify, no later
        // frame is touched either.
        self.pending_role = false;

        // Frames carrying anything but `data:` lines are re-emitted as-is
        // rather than rebuilt, so nothing the upstream sent can be lost.
        if !is_data_only(frame) {
            return format!("{frame}\n\n");
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&payload) else {
            return format!("{frame}\n\n");
        };
        if !inject_role(&mut value) {
            return format!("{frame}\n\n");
        }
        match serde_json::to_string(&value) {
            Ok(rewritten) => format!("data: {rewritten}\n\n"),
            Err(_) => format!("{frame}\n\n"),
        }
    }
}

/// Holds a whole non-SSE body and hands it back with the original tool names
/// restored. Selected only for a response that is not a stream, so the
/// buffering rule at the top of this module still holds.
///
/// Over [`MAX_TRANSFORM_BUFFER_BYTES`] it fails open: what has been buffered is
/// released untransformed and the rest of the body is forwarded verbatim. An
/// unrestored name is a worse answer than a restored one, and a far better one
/// than a failed request.
pub struct BufferedRestore {
    names: NameMap,
    buf: Vec<u8>,
    /// Over the cap: everything from here is forwarded verbatim.
    raw: bool,
}

impl BufferedRestore {
    fn new(names: NameMap) -> Self {
        Self {
            names,
            buf: Vec::new(),
            raw: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if self.raw {
            return bytes.to_vec();
        }
        self.buf.extend_from_slice(bytes);
        if self.buf.len() <= MAX_TRANSFORM_BUFFER_BYTES {
            return Vec::new();
        }
        tracing::warn!(
            "response body exceeded {MAX_TRANSFORM_BUFFER_BYTES} bytes; forwarding it without restoring tool names"
        );
        self.raw = true;
        std::mem::take(&mut self.buf)
    }

    fn finish(&mut self) -> Vec<u8> {
        if self.raw {
            return Vec::new();
        }
        self.raw = true;
        self.names.restore_bytes(std::mem::take(&mut self.buf))
    }
}

/// True when every non-empty line of the frame is a `data:` line.
fn is_data_only(frame: &str) -> bool {
    frame
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.trim().is_empty())
        .all(|line| line.starts_with("data:"))
}

/// Adds `role:"assistant"` to `choices[0].delta`. Returns false — leaving the
/// frame untouched — whenever any guard fails, including an existing role,
/// which is never overwritten.
fn inject_role(value: &mut Value) -> bool {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return false;
    };
    let Some(first) = choices.first_mut() else {
        return false;
    };
    let Some(delta) = first.get_mut("delta").and_then(Value::as_object_mut) else {
        return false;
    };
    if delta.contains_key("role") {
        return false;
    }
    delta.insert("role".to_string(), Value::String(ASSISTANT.to_string()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).unwrap()
    }

    fn chunk(content: &str) -> String {
        format!(
            "data: {{\"id\":\"c1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n"
        )
    }

    #[test]
    fn injects_role_into_first_delta_only() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        let first = text(transform.push(chunk("a").as_bytes()));
        assert!(first.contains("\"role\":\"assistant\""), "{first}");

        let second = text(transform.push(chunk("b").as_bytes()));
        assert!(!second.contains("role"), "{second}");
        assert_eq!(second, chunk("b"));
        assert!(transform.finish().is_empty());
    }

    #[test]
    fn never_overwrites_an_existing_role() {
        let frame = "data: {\"choices\":[{\"delta\":{\"role\":\"tool\",\"content\":\"x\"}}]}\n\n";
        let mut transform = BodyTransform::sse(true, NameMap::default());
        assert_eq!(text(transform.push(frame.as_bytes())), frame);
    }

    #[test]
    fn ignores_frames_without_choices_or_delta() {
        for frame in [
            "data: {\"choices\":[]}\n\n",
            "data: {\"id\":\"c1\"}\n\n",
            "data: {\"choices\":[{\"index\":0}]}\n\n",
            "data: [DONE]\n\n",
            "data: not json\n\n",
        ] {
            let mut transform = BodyTransform::sse(true, NameMap::default());
            assert_eq!(text(transform.push(frame.as_bytes())), frame, "{frame}");
        }
    }

    #[test]
    fn a_comment_frame_does_not_consume_the_one_chance() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        let ping = ": keepalive\n\n";
        assert_eq!(text(transform.push(ping.as_bytes())), ping);
        let out = text(transform.push(chunk("a").as_bytes()));
        assert!(out.contains("\"role\":\"assistant\""), "{out}");
    }

    #[test]
    fn frames_with_non_data_lines_pass_through_unrebuilt() {
        let frame = "event: delta\ndata: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let mut transform = BodyTransform::sse(true, NameMap::default());
        assert_eq!(text(transform.push(frame.as_bytes())), frame);
    }

    #[test]
    fn disengages_to_raw_passthrough_after_the_first_frame() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        transform.push(chunk("a").as_bytes());
        // Not SSE-shaped at all: proof that no framing is happening any more.
        let raw = b"\x00\x01 arbitrary bytes";
        assert_eq!(transform.push(raw), raw.to_vec());
    }

    #[test]
    fn holds_a_partial_frame_then_flushes_it_on_disengage() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        assert!(transform.push(b"data: {\"choices\"").is_empty());
        let out = text(transform.push(b":[{\"delta\":{\"content\":\"x\"}}]}\n\ndata: tail"));
        assert!(out.contains("\"role\":\"assistant\""), "{out}");
        assert!(out.ends_with("data: tail"), "partial frame flushed: {out}");
        assert!(transform.finish().is_empty());
    }

    #[test]
    fn finish_flushes_a_truncated_frame_and_is_idempotent() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        assert!(transform.push(b"data: {\"choices\"").is_empty());
        assert_eq!(text(transform.finish()), "data: {\"choices\"");
        assert!(transform.finish().is_empty());
    }

    #[test]
    fn done_sentinel_passes_through() {
        let mut transform = BodyTransform::sse(true, NameMap::default());
        transform.push(chunk("a").as_bytes());
        let done = "data: [DONE]\n\n";
        assert_eq!(text(transform.push(done.as_bytes())), done);
    }

    #[test]
    fn reports_no_observed_usage() {
        assert!(
            BodyTransform::sse(true, NameMap::default())
                .observed_usage()
                .is_none()
        );
        assert!(
            BodyTransform::name_restore(NameMap::default())
                .observed_usage()
                .is_none()
        );
    }

    // --- tool-name restoration ---

    /// 79 bytes, so it is aliased; the mapping is built the same way a real
    /// request builds it.
    const LONG_TOOL: &str =
        "mcp__github_enterprise__very_long_tool_name_for_listing_pull_request_reviews";

    fn tool_names() -> (NameMap, String) {
        let mut body = serde_json::json!({"tools": [{"function": {"name": LONG_TOOL}}]});
        let names = crate::services::tool_names::truncate_in_body(body.as_object_mut().unwrap());
        let alias = body["tools"][0]["function"]["name"]
            .as_str()
            .unwrap()
            .to_string();
        (names, alias)
    }

    #[test]
    fn buffered_restore_emits_nothing_until_finish() {
        let (names, alias) = tool_names();
        let mut transform = BodyTransform::name_restore(names);
        let body = format!("{{\"tool_calls\":[{{\"function\":{{\"name\":\"{alias}\"}}}}]}}");
        assert!(transform.push(&body.as_bytes()[..10]).is_empty());
        assert!(transform.push(&body.as_bytes()[10..]).is_empty());
        let out = text(transform.finish());
        assert!(out.contains(&format!("\"name\":\"{LONG_TOOL}\"")), "{out}");
        assert!(transform.finish().is_empty(), "finish is idempotent");
    }

    #[test]
    fn buffered_restore_is_byte_identity_without_a_mapping() {
        let mut transform = BodyTransform::name_restore(NameMap::default());
        let body = br#"{"choices":[{"message":{"content":"ok"}}]}"#;
        assert!(transform.push(body).is_empty());
        assert_eq!(transform.finish(), body.to_vec());
    }

    #[test]
    fn buffered_restore_over_the_cap_falls_back_to_raw_passthrough() {
        let (names, alias) = tool_names();
        let mut transform = BodyTransform::name_restore(names);
        let filler = vec![b'x'; MAX_TRANSFORM_BUFFER_BYTES + 1];
        // Everything buffered so far is released untransformed...
        assert_eq!(transform.push(&filler).len(), filler.len());
        // ...and the rest of the body streams through verbatim, alias and all.
        let tail = format!("\"{alias}\"");
        assert_eq!(text(transform.push(tail.as_bytes())), tail);
        assert!(transform.finish().is_empty());
    }

    #[test]
    fn sse_restores_names_in_any_frame_not_just_the_first() {
        let (names, alias) = tool_names();
        let mut transform = BodyTransform::sse(true, names);
        let first = text(transform.push(chunk("a").as_bytes()));
        assert!(first.contains("\"role\":\"assistant\""), "{first}");

        let call = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"name\":\"{alias}\"}}}}]}}}}]}}\n\n"
        );
        let out = text(transform.push(call.as_bytes()));
        assert!(out.contains(&format!("\"name\":\"{LONG_TOOL}\"")), "{out}");
        assert!(!out.contains(&alias), "{out}");
    }

    #[test]
    fn sse_stays_engaged_for_the_whole_stream_when_names_are_mapped() {
        let (names, alias) = tool_names();
        let mut transform = BodyTransform::sse(true, names);
        transform.push(chunk("a").as_bytes());
        // An alias split across two chunks is still restored, because framing
        // continues: a frame boundary is the only place it cannot straddle.
        let call = format!("data: {{\"name\":\"{alias}\"}}\n\n");
        let (head, tail) = call.split_at(call.len() / 2);
        assert!(transform.push(head.as_bytes()).is_empty());
        let out = text(transform.push(tail.as_bytes()));
        assert_eq!(out, format!("data: {{\"name\":\"{LONG_TOOL}\"}}\n\n"));
    }

    #[test]
    fn sse_without_role_injection_still_restores_names() {
        let (names, alias) = tool_names();
        let mut transform = BodyTransform::sse(false, names);
        let frame = format!("data: {{\"name\":\"{alias}\"}}\n\n");
        let out = text(transform.push(frame.as_bytes()));
        assert!(out.contains(LONG_TOOL), "{out}");
        assert!(!out.contains("\"role\""), "no repair was asked for: {out}");
    }
}

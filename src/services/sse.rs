//! Incremental Server-Sent-Events framing.
//!
//! Shared by every response transform that has to look inside a stream. The
//! split is the reason those transforms are allowed to touch an SSE body at
//! all: a frame is the largest unit any of them ever holds, so nothing is
//! buffered beyond the frame currently being assembled.

use serde_json::Value;

/// Splits a byte stream into complete `\n\n`-delimited SSE frames as they
/// arrive, holding back a trailing partial frame until its terminator lands.
#[derive(Default)]
pub struct SseFrames {
    buf: Vec<u8>,
}

impl SseFrames {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds raw upstream bytes and returns every frame that is now complete.
    /// Blank filler frames are dropped; the caller only ever sees content.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some((end, terminator)) = frame_end(&self.buf) {
            let frame = String::from_utf8_lossy(&self.buf[..end]).into_owned();
            self.buf.drain(..end + terminator);
            if !frame.trim().is_empty() {
                frames.push(frame);
            }
        }
        frames
    }

    /// Takes whatever incomplete frame is still buffered, leaving the splitter
    /// empty. Used by transforms that stop framing mid-stream (either because
    /// they are done rewriting or because the upstream ended) and must not
    /// swallow the bytes they were holding back.
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// The `data:` payload of one frame, with the optional single leading space
/// stripped and multiple `data:` lines joined by newlines, per the SSE spec.
/// Frames carrying no `data:` line at all (comments, bare `event:`) yield
/// `None`.
#[must_use]
pub fn parse_data_frame(frame: &str) -> Option<String> {
    let mut data: Option<String> = None;
    for raw in frame.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        match data.as_mut() {
            Some(acc) => {
                acc.push('\n');
                acc.push_str(rest);
            }
            None => data = Some(rest.to_string()),
        }
    }
    data
}

/// Formats one SSE event. The `event:` line is mandatory for Anthropic
/// clients — their SDKs dispatch on it rather than on the payload's `type`.
#[must_use]
pub fn format_event(event: &str, data: &Value) -> String {
    let payload = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {event}\ndata: {payload}\n\n")
}

/// Returns `(frame_length, terminator_length)` for the first frame boundary.
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        let rest = &buf[i..];
        if rest.starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        if rest.starts_with(b"\n\n") || rest.starts_with(b"\r\r") {
            return Some((i, 2));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_frames_on_blank_line() {
        let mut frames = SseFrames::new();
        let out = frames.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(out, vec!["data: one".to_string(), "data: two".to_string()]);
    }

    #[test]
    fn holds_partial_frame_until_terminator() {
        let mut frames = SseFrames::new();
        assert!(frames.push(b"data: {\"a\":").is_empty());
        assert!(frames.push(b"1}").is_empty());
        assert_eq!(frames.push(b"\n\n"), vec!["data: {\"a\":1}".to_string()]);
    }

    #[test]
    fn handles_crlf_and_multiple_frames_in_one_chunk() {
        let mut frames = SseFrames::new();
        let out = frames.push(b"event: x\r\ndata: one\r\n\r\ndata: two\n\n\n\n");
        assert_eq!(
            out,
            vec!["event: x\r\ndata: one".to_string(), "data: two".to_string()]
        );
    }

    #[test]
    fn parses_data_lines_and_joins_multiples() {
        assert_eq!(parse_data_frame("data: hello"), Some("hello".to_string()));
        assert_eq!(parse_data_frame("data:hello"), Some("hello".to_string()));
        assert_eq!(
            parse_data_frame("event: m\ndata: a\ndata: b"),
            Some("a\nb".to_string())
        );
        assert_eq!(parse_data_frame(": keepalive"), None);
    }

    #[test]
    fn format_event_emits_event_and_data_lines() {
        let out = format_event("message_stop", &json!({"type": "message_stop"}));
        assert_eq!(
            out,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }
}

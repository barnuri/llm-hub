//! Copilot's `/responses` stream tags the events of one output item with differing item ids, which
//! breaks clients that key streaming state by item id. This pins one id per `output_index`.

use std::collections::HashMap;

use serde_json::Value;

use crate::services::sse::{SseFrames, parse_data_frame};

const OUTPUT_INDEX_KEY: &str = "\"output_index\"";
const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
const OUTPUT_ITEM_DONE: &str = "response.output_item.done";

/// Rewrites each event's item id to the first id seen for its `output_index`. Holds at most one
/// partial frame; anything that is not a JSON event with an `output_index` passes through as-is.
#[derive(Default)]
pub struct ResponsesItemIds {
    frames: SseFrames,
    stable_ids: HashMap<u64, String>,
}

impl std::fmt::Debug for ResponsesItemIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesItemIds")
            .field("stable_ids", &self.stable_ids)
            .finish_non_exhaustive()
    }
}

impl ResponsesItemIds {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds upstream bytes and returns every complete frame, rewritten where needed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for frame in self.frames.push(chunk) {
            if let Some(rewritten) = self.rewrite(&frame) {
                out.extend_from_slice(rewritten.as_bytes());
            } else {
                out.extend_from_slice(frame.as_bytes());
                out.extend_from_slice(b"\n\n");
            }
        }
        out
    }

    /// Releases the unterminated trailing frame, if any, untouched.
    pub fn finish(&mut self) -> Vec<u8> {
        self.frames.take_pending()
    }

    /// The rewritten frame with its terminator, or `None` when the frame stays as it was.
    fn rewrite(&mut self, frame: &str) -> Option<String> {
        let payload = parse_data_frame(frame)?;
        if !payload.contains(OUTPUT_INDEX_KEY) {
            return None;
        }
        let mut event = serde_json::from_str::<Value>(&payload).ok()?;
        if !self.pin_item_id(&mut event) {
            return None;
        }
        let rewritten = serde_json::to_string(&event).ok()?;
        Some(replace_data(frame, &rewritten))
    }

    /// Returns true when the event's id was changed to the stable one.
    fn pin_item_id(&mut self, event: &mut Value) -> bool {
        let Some(output_index) = event.get("output_index").and_then(Value::as_u64) else {
            return false;
        };
        let event_type = event.get("type").and_then(Value::as_str);
        let is_item_event = matches!(event_type, Some(OUTPUT_ITEM_ADDED | OUTPUT_ITEM_DONE));
        let id_slot = if event.get("item_id").is_some_and(Value::is_string) {
            event.get_mut("item_id")
        } else if is_item_event {
            event.get_mut("item").and_then(|item| item.get_mut("id"))
        } else {
            None
        };
        let Some(Value::String(id)) = id_slot else {
            return false;
        };
        let stable = self
            .stable_ids
            .entry(output_index)
            .or_insert_with(|| id.clone());
        if stable == id {
            return false;
        }
        id.clone_from(stable);
        true
    }
}

/// Swaps the frame's `data:` lines for one line carrying `payload`, keeping every other line.
fn replace_data(frame: &str, payload: &str) -> String {
    let mut out = String::with_capacity(frame.len() + payload.len());
    let mut data_written = false;
    for raw in frame.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("data:") {
            if !data_written {
                out.push_str("data: ");
                out.push_str(payload);
                out.push('\n');
                data_written = true;
            }
            continue;
        }
        out.push_str(raw);
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, data: &Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn data_of(frame: &str) -> Value {
        serde_json::from_str(&parse_data_frame(frame).unwrap()).unwrap()
    }

    fn frames(bytes: &[u8]) -> Vec<String> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter(|frame| !frame.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn added(index: u64, id: &str) -> String {
        event(
            OUTPUT_ITEM_ADDED,
            &json!({"type": OUTPUT_ITEM_ADDED, "output_index": index, "item": {"id": id, "type": "message"}}),
        )
    }

    fn delta(index: u64, id: &str, text: &str) -> String {
        event(
            "response.output_text.delta",
            &json!({"type": "response.output_text.delta", "output_index": index, "item_id": id, "delta": text}),
        )
    }

    #[test]
    fn later_events_get_first_id_for_their_output_index() {
        let mut ids = ResponsesItemIds::new();
        let stream = [
            added(0, "msg_a"),
            delta(0, "msg_b", "hi"),
            delta(0, "msg_c", "!"),
        ]
        .concat();

        let out = frames(&ids.push(stream.as_bytes()));

        assert_eq!(out.len(), 3);
        assert_eq!(data_of(&out[1])["item_id"], "msg_a");
        assert_eq!(data_of(&out[2])["item_id"], "msg_a");
        assert_eq!(data_of(&out[2])["delta"], "!");
        assert!(out[1].starts_with("event: response.output_text.delta\n"));
    }

    #[test]
    fn output_item_done_gets_stable_item_id() {
        let mut ids = ResponsesItemIds::new();
        let done = event(
            OUTPUT_ITEM_DONE,
            &json!({"type": OUTPUT_ITEM_DONE, "output_index": 0, "item": {"id": "msg_z", "status": "completed"}}),
        );

        ids.push(added(0, "msg_a").as_bytes());
        let out = frames(&ids.push(done.as_bytes()));

        assert_eq!(data_of(&out[0])["item"]["id"], "msg_a");
        assert_eq!(data_of(&out[0])["item"]["status"], "completed");
    }

    #[test]
    fn first_item_id_wins_without_output_item_added() {
        let mut ids = ResponsesItemIds::new();
        let stream = [delta(0, "msg_b", "a"), delta(0, "msg_c", "b")].concat();

        let out = frames(&ids.push(stream.as_bytes()));

        assert_eq!(data_of(&out[1])["item_id"], "msg_b");
    }

    #[test]
    fn output_indexes_keep_separate_ids() {
        let mut ids = ResponsesItemIds::new();
        let stream = [
            added(0, "rs_a"),
            added(1, "msg_a"),
            delta(1, "msg_x", "t"),
            delta(0, "rs_y", "r"),
        ]
        .concat();

        let out = frames(&ids.push(stream.as_bytes()));

        assert_eq!(data_of(&out[2])["item_id"], "msg_a");
        assert_eq!(data_of(&out[3])["item_id"], "rs_a");
    }

    #[test]
    fn events_split_across_chunks_are_rewritten() {
        let mut ids = ResponsesItemIds::new();
        let stream = [added(0, "msg_a"), delta(0, "msg_b", "hi")].concat();
        let bytes = stream.as_bytes();

        let mut out = Vec::new();
        for chunk in bytes.chunks(7) {
            out.extend(ids.push(chunk));
        }
        out.extend(ids.finish());

        let out = frames(&out);
        assert_eq!(out.len(), 2);
        assert_eq!(data_of(&out[1])["item_id"], "msg_a");
    }

    #[test]
    fn unchanged_and_unrelated_frames_pass_through_verbatim() {
        let mut ids = ResponsesItemIds::new();
        let stream = [
            added(0, "msg_a"),
            delta(0, "msg_a", "same"),
            ": keepalive\n\n".to_string(),
            "data: not json \"output_index\"\n\n".to_string(),
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();

        let out = ids.push(stream.as_bytes());

        assert_eq!(String::from_utf8(out).unwrap(), stream);
    }

    #[test]
    fn rewrite_keeps_crlf_lines_and_other_fields() {
        let mut ids = ResponsesItemIds::new();
        ids.push(added(0, "msg_a").as_bytes());
        let frame = "event: response.output_text.delta\r\nid: 7\r\ndata: {\"output_index\":0,\"item_id\":\"msg_b\"}\r\n\r\n";

        let out = String::from_utf8(ids.push(frame.as_bytes())).unwrap();

        assert!(out.starts_with("event: response.output_text.delta\r\nid: 7\r\ndata: {"));
        assert!(out.ends_with("}\n\n"));
        assert_eq!(
            data_of(&out),
            json!({"output_index": 0, "item_id": "msg_a"})
        );
    }

    #[test]
    fn finish_releases_unterminated_tail() {
        let mut ids = ResponsesItemIds::new();

        assert!(ids.push(b"data: {\"output_index\":0").is_empty());
        assert_eq!(ids.finish(), b"data: {\"output_index\":0".to_vec());
        assert!(ids.finish().is_empty());
    }
}

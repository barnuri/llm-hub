//! The model calls of media fallback: an image caption from a vision model and an audio
//! transcript from a speech model. Both go through the profile's pooled upstream client.

use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use serde_json::{Value, json};

use crate::dependencies::state::AppState;
use crate::routes::proxy;
use crate::schemas::model_id::ModelId;

/// The client path of a caption call.
const CHAT_PATH: &str = "/v1/chat/completions";
/// The client path of a transcript call.
const TRANSCRIPTION_PATH: &str = "/v1/audio/transcriptions";
/// The instruction sent with each image.
pub(crate) const CAPTION_PROMPT: &str = "Describe this image factually and concisely for a reader who cannot see it. Include any visible text verbatim.";
/// The longest caption, in tokens.
const CAPTION_MAX_TOKENS: u64 = 300;
/// The multipart boundary of a transcript call. Audio bytes that hold it would end a part early,
/// so the boundary is long and random-looking.
const BOUNDARY: &str = "llm-hub-media-fallback-7f3a9c2e5b1d";

/// Asks `model` to describe the image at `data_uri`.
///
/// # Errors
/// The failed call, or a reply with no text.
pub(crate) async fn caption(
    state: &AppState,
    model: &ModelId,
    data_uri: &str,
    timeout: Duration,
) -> Result<String, String> {
    let body = json!({
        "model": proxy::upstream_key(state, model),
        "max_tokens": CAPTION_MAX_TOKENS,
        "stream": false,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": CAPTION_PROMPT},
            {"type": "image_url", "image_url": {"url": data_uri}}
        ]}]
    });
    let raw = proxy::send_media_call(
        state,
        model,
        CHAT_PATH,
        "application/json",
        Bytes::from(body.to_string()),
        timeout,
    )
    .await?;
    let reply: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad reply: {e}"))?;
    non_empty(&completion_text(&reply), "the caption is empty")
}

/// Asks `model` for the transcript of base64 `data` in audio `format`.
///
/// # Errors
/// Data that is not base64, the failed call, or a reply with no text.
pub(crate) async fn transcribe(
    state: &AppState,
    model: &ModelId,
    data: &str,
    format: &str,
    timeout: Duration,
) -> Result<String, String> {
    let audio = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("the audio is not base64: {e}"))?;
    let body = multipart_body(&proxy::upstream_key(state, model), format, &audio);
    let raw = proxy::send_media_call(
        state,
        model,
        TRANSCRIPTION_PATH,
        &format!("multipart/form-data; boundary={BOUNDARY}"),
        Bytes::from(body),
        timeout,
    )
    .await?;
    let reply: Value = serde_json::from_slice(&raw).map_err(|e| format!("bad reply: {e}"))?;
    let text = reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    non_empty(text, "the transcript is empty")
}

/// An `OpenAI` transcription form: the model, a JSON reply format, and the audio file.
fn multipart_body(model: &str, format: &str, audio: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(audio.len() + 512);
    for (name, value) in [("model", model), ("response_format", "json")] {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.{format}\"\r\nContent-Type: audio/{format}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// The text of the first choice: a string, or the text parts of a part list.
fn completion_text(reply: &Value) -> String {
    match reply.pointer("/choices/0/message/content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn non_empty(text: &str, empty: &str) -> Result<String, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Err(empty.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_text_reads_a_string_or_text_parts() {
        let reply = json!({"choices": [{"message": {"content": "a cat"}}]});
        assert_eq!(completion_text(&reply), "a cat");
        let reply = json!({"choices": [{"message": {"content": [
            {"type": "text", "text": "a"}, {"type": "image"}, {"type": "text", "text": "b"}
        ]}}]});
        assert_eq!(completion_text(&reply), "a\nb");
        assert_eq!(completion_text(&json!({})), "");
    }

    #[test]
    fn the_multipart_form_carries_the_model_and_the_audio() {
        let body = multipart_body("whisper-1", "mp3", b"\x01\x02");
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"),
            "{text}"
        );
        assert!(
            text.contains("filename=\"audio.mp3\"\r\nContent-Type: audio/mp3"),
            "{text}"
        );
        assert!(body.windows(2).any(|pair| pair == b"\x01\x02"));
        assert!(text.ends_with(&format!("--{BOUNDARY}--\r\n")));
    }

    #[test]
    fn blank_text_is_an_error() {
        assert_eq!(non_empty("  ", "empty"), Err("empty".to_owned()));
        assert_eq!(non_empty(" x ", "empty"), Ok("x".to_owned()));
    }
}

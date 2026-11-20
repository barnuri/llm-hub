//! Finds the media parts of a request body and swaps each one for a text part. Covers the three
//! request shapes, at any depth, so parts inside tool results are found too.

use serde_json::Value;

use crate::services::request_images::BodyShape;

/// The media a part carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MediaKind {
    Image,
    Audio,
    Document,
}

impl MediaKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Audio => "audio",
            MediaKind::Document => "document",
        }
    }
}

/// Where a part's media is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MediaSource {
    /// Base64 data with its media type, from a data URI or a base64 source block.
    Inline { media_type: String, data: String },
    /// A remote URL. The hub never fetches it.
    Url(String),
    /// A document that is already plain text.
    Text(String),
    /// An upstream file id, which only the upstream can read.
    Reference(String),
}

/// One media part of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MediaItem {
    pub(crate) kind: MediaKind,
    pub(crate) source: MediaSource,
    pub(crate) filename: Option<String>,
}

/// The part types that carry media in `shape`.
fn media_kind(shape: BodyShape, part_type: &str) -> Option<MediaKind> {
    match (shape, part_type) {
        (BodyShape::ChatCompletions, "image_url")
        | (BodyShape::Responses, "input_image")
        | (BodyShape::AnthropicMessages, "image") => Some(MediaKind::Image),
        (BodyShape::ChatCompletions | BodyShape::Responses, "input_audio") => {
            Some(MediaKind::Audio)
        }
        (BodyShape::ChatCompletions, "file")
        | (BodyShape::Responses, "input_file")
        | (BodyShape::AnthropicMessages, "document") => Some(MediaKind::Document),
        _ => None,
    }
}

/// Whether the body of `shape` holds a media part anywhere. Strings are never scanned.
pub(crate) fn has_media(shape: BodyShape, body: &Value) -> bool {
    match body {
        Value::Object(fields) => {
            part_kind(shape, body).is_some() || fields.values().any(|field| has_media(shape, field))
        }
        Value::Array(items) => items.iter().any(|item| has_media(shape, item)),
        _ => false,
    }
}

/// Every media part of `body`, in the order [`replace_media`] visits them.
pub(crate) fn collect_media(shape: BodyShape, body: &Value) -> Vec<MediaItem> {
    let mut found = Vec::new();
    visit(shape, body, &mut |part, kind| {
        found.push(media_item(part, kind));
    });
    found
}

/// Replaces the n-th media part with a text part holding `texts[n]`. Parts past the end of
/// `texts` stay as they are.
pub(crate) fn replace_media(shape: BodyShape, body: &mut Value, texts: &[String]) {
    let mut next = texts.iter();
    visit_mut(shape, body, &mut |part| {
        if let Some(text) = next.next() {
            *part = text_part(shape, text);
        }
    });
}

fn part_kind(shape: BodyShape, part: &Value) -> Option<MediaKind> {
    part.get("type")
        .and_then(Value::as_str)
        .and_then(|part_type| media_kind(shape, part_type))
}

fn visit(shape: BodyShape, value: &Value, found: &mut impl FnMut(&Value, MediaKind)) {
    if let Some(kind) = part_kind(shape, value) {
        found(value, kind);
        return;
    }
    match value {
        Value::Array(items) => items.iter().for_each(|item| visit(shape, item, found)),
        Value::Object(fields) => fields.values().for_each(|field| visit(shape, field, found)),
        _ => {}
    }
}

fn visit_mut(shape: BodyShape, value: &mut Value, found: &mut impl FnMut(&mut Value)) {
    if part_kind(shape, value).is_some() {
        found(value);
        return;
    }
    match value {
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| visit_mut(shape, item, found)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| visit_mut(shape, field, found)),
        _ => {}
    }
}

/// The text part of `shape`: `input_text` on Responses, `text` elsewhere.
fn text_part(shape: BodyShape, text: &str) -> Value {
    let part_type = match shape {
        BodyShape::Responses => "input_text",
        BodyShape::ChatCompletions | BodyShape::AnthropicMessages => "text",
    };
    serde_json::json!({ "type": part_type, "text": text })
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn media_item(part: &Value, kind: MediaKind) -> MediaItem {
    let source = match kind {
        MediaKind::Image => image_source(part),
        MediaKind::Audio => audio_source(part),
        MediaKind::Document => document_source(part),
    };
    let filename = ["/file/filename", "/filename", "/title"]
        .into_iter()
        .find_map(|pointer| str_at(part, pointer))
        .map(str::to_owned);
    MediaItem {
        kind,
        source,
        filename,
    }
}

fn image_source(part: &Value) -> MediaSource {
    let url = str_at(part, "/image_url/url").or_else(|| str_at(part, "/image_url"));
    if let Some(url) = url {
        return from_uri(url);
    }
    if let Some(source) = part.get("source") {
        return block_source(source);
    }
    MediaSource::Reference(str_at(part, "/file_id").unwrap_or_default().to_owned())
}

fn audio_source(part: &Value) -> MediaSource {
    let data = str_at(part, "/input_audio/data").unwrap_or_default();
    let format = str_at(part, "/input_audio/format").unwrap_or("wav");
    MediaSource::Inline {
        media_type: format!("audio/{format}"),
        data: data.to_owned(),
    }
}

fn document_source(part: &Value) -> MediaSource {
    if let Some(source) = part.get("source") {
        return block_source(source);
    }
    if let Some(data) = str_at(part, "/file/file_data").or_else(|| str_at(part, "/file_data")) {
        return match data.strip_prefix("data:") {
            Some(_) => from_uri(data),
            None => MediaSource::Inline {
                media_type: "application/pdf".to_owned(),
                data: data.to_owned(),
            },
        };
    }
    if let Some(url) = str_at(part, "/file_url") {
        return MediaSource::Url(url.to_owned());
    }
    let id = str_at(part, "/file/file_id").or_else(|| str_at(part, "/file_id"));
    MediaSource::Reference(id.unwrap_or_default().to_owned())
}

/// An Anthropic `source` block: base64, url, plain text, nested content or a file id.
fn block_source(source: &Value) -> MediaSource {
    let media_type = str_at(source, "/media_type").unwrap_or_default().to_owned();
    match str_at(source, "/type") {
        Some("base64") => MediaSource::Inline {
            media_type,
            data: str_at(source, "/data").unwrap_or_default().to_owned(),
        },
        Some("url") => MediaSource::Url(str_at(source, "/url").unwrap_or_default().to_owned()),
        Some("text") => MediaSource::Text(str_at(source, "/data").unwrap_or_default().to_owned()),
        Some("content") => MediaSource::Text(content_text(source.get("content"))),
        _ => MediaSource::Reference(str_at(source, "/file_id").unwrap_or_default().to_owned()),
    }
}

/// The text blocks of an Anthropic document's `content`, joined by newlines.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| str_at(block, "/text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A `data:<type>;base64,<data>` URI as inline data, any other text as a URL.
fn from_uri(uri: &str) -> MediaSource {
    if let Some(rest) = uri.strip_prefix("data:")
        && let Some((header, data)) = rest.split_once(',')
        && let Some(media_type) = header.strip_suffix(";base64")
    {
        return MediaSource::Inline {
            media_type: media_type.to_ascii_lowercase(),
            data: data.to_owned(),
        };
    }
    MediaSource::Url(uri.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn inline(media_type: &str, data: &str) -> MediaSource {
        MediaSource::Inline {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        }
    }

    #[test]
    fn chat_parts_are_found_in_tool_messages_and_replaced_with_text() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/PNG;base64,AAAA"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": [
                {"type": "input_audio", "input_audio": {"data": "BBBB", "format": "mp3"}},
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,CCCC", "filename": "a.pdf"}}
            ]}
        ]});
        let shape = BodyShape::ChatCompletions;
        assert!(has_media(shape, &body));

        let items = collect_media(shape, &body);
        let sources: Vec<_> = items.iter().map(|item| item.source.clone()).collect();
        assert_eq!(
            sources,
            [
                inline("image/png", "AAAA"),
                inline("audio/mp3", "BBBB"),
                inline("application/pdf", "CCCC")
            ]
        );
        assert_eq!(items[2].filename.as_deref(), Some("a.pdf"));
        assert_eq!(items[1].kind, MediaKind::Audio);

        replace_media(
            shape,
            &mut body,
            &["one".into(), "two".into(), "three".into()],
        );
        assert_eq!(
            body["messages"][0]["content"][1],
            json!({"type": "text", "text": "one"})
        );
        assert_eq!(
            body["messages"][1]["content"][1],
            json!({"type": "text", "text": "three"})
        );
        assert!(!has_media(shape, &body));
    }

    #[test]
    fn responses_parts_become_input_text() {
        let mut body = json!({"input": [
            {"type": "function_call_output", "call_id": "c1", "output": [
                {"type": "input_image", "image_url": "https://example.com/a.png"},
                {"type": "input_file", "file_id": "file-1"},
                {"type": "input_file", "file_url": "https://example.com/a.pdf"},
                {"type": "input_file", "file_data": "RAW", "filename": "b.pdf"}
            ]}
        ]});
        let shape = BodyShape::Responses;
        let items = collect_media(shape, &body);
        let sources: Vec<_> = items.iter().map(|item| item.source.clone()).collect();
        assert_eq!(
            sources,
            [
                MediaSource::Url("https://example.com/a.png".into()),
                MediaSource::Reference("file-1".into()),
                MediaSource::Url("https://example.com/a.pdf".into()),
                inline("application/pdf", "RAW"),
            ]
        );

        replace_media(shape, &mut body, &["x".into()]);
        assert_eq!(
            body["input"][0]["output"][0],
            json!({"type": "input_text", "text": "x"})
        );
        assert_eq!(body["input"][0]["output"][1]["type"], "input_file");
    }

    #[test]
    fn anthropic_blocks_read_every_source_kind() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AA"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/i.jpg"}},
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "document", "title": "notes", "source": {"type": "text", "media_type": "text/plain", "data": "hello"}},
                {"type": "document", "source": {"type": "content", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}},
                {"type": "document", "source": {"type": "content", "content": "plain"}},
                {"type": "document", "source": {"type": "file", "file_id": "f1"}}
            ]}
        ]}]});
        let items = collect_media(BodyShape::AnthropicMessages, &body);
        let sources: Vec<_> = items.iter().map(|item| item.source.clone()).collect();
        assert_eq!(
            sources,
            [
                inline("image/jpeg", "AA"),
                MediaSource::Url("https://example.com/i.jpg".into()),
                MediaSource::Text("hello".into()),
                MediaSource::Text("a\nb".into()),
                MediaSource::Text("plain".into()),
                MediaSource::Reference("f1".into()),
            ]
        );
        assert_eq!(items[2].filename.as_deref(), Some("notes"));
    }

    #[test]
    fn a_part_type_of_another_shape_or_inside_a_string_is_not_media() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "input_image", "image_url": "x"}]},
            {"role": "user", "content": "{\"type\": \"image_url\"}"}
        ]});
        assert!(!has_media(BodyShape::ChatCompletions, &body));
        assert!(collect_media(BodyShape::ChatCompletions, &body).is_empty());
    }

    #[test]
    fn odd_image_and_document_parts_fall_back_to_references() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": "data:image/png,notbase64"},
            {"type": "image_url"},
            {"type": "file", "file": {"file_id": "file-9"}},
            {"type": "input_audio", "input_audio": {}}
        ]}]});
        let items = collect_media(BodyShape::ChatCompletions, &body);
        let sources: Vec<_> = items.iter().map(|item| item.source.clone()).collect();
        assert_eq!(
            sources,
            [
                MediaSource::Url("data:image/png,notbase64".into()),
                MediaSource::Reference(String::new()),
                MediaSource::Reference("file-9".into()),
                inline("audio/wav", ""),
            ]
        );
        assert_eq!(MediaKind::Document.as_str(), "document");
    }
}

//! Media fallback: before an attempt goes to a model that cannot read images, audio or files,
//! each such part becomes text. An image becomes a caption from a vision model, audio a
//! transcript from a speech model, and a PDF its text layer. Off by default.

use std::collections::{HashMap, HashSet};

use base64::Engine;
use bytes::Bytes;
use futures_util::future::join_all;
use serde_json::Value;

use crate::configs::MediaFallbackConfig;
use crate::dependencies::state::AppState;
use crate::routes::proxy;
use crate::schemas::model_id::ModelId;
use crate::services::request_images::BodyShape;
use crate::services::store::now_ms;
use crate::utils::glob::glob_matches;

mod media_cache;
mod media_calls;
mod media_parts;
mod pdf_text;

pub(crate) use media_cache::MediaCache;
use media_cache::media_key;
use media_parts::{MediaItem, MediaKind, MediaSource, collect_media, has_media, replace_media};

/// The image types a vision model reads.
const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];
/// The cache model name of PDF text, which no model makes.
const PDF_EXTRACTOR: &str = "builtin/pdf-text";
/// The longest document text sent in place of a document, in characters.
const MAX_DOCUMENT_CHARS: usize = 100_000;

/// Whether the request body holds media of any kind that media fallback converts.
#[must_use]
pub(crate) fn request_has_media(path: &str, body: &Value) -> bool {
    BodyShape::from_path(path).is_some_and(|shape| has_media(shape, body))
}

/// Whether `model_id` gets text instead of media: a model in the configured text-only globs, one
/// whose descriptor reports no vision, or one that refused an image before.
#[must_use]
pub(crate) fn targets_text_only(
    state: &AppState,
    config: &MediaFallbackConfig,
    model_id: &ModelId,
) -> bool {
    let qualified = model_id.qualified();
    config
        .text_only
        .iter()
        .any(|glob| glob_matches(glob, &qualified))
        || proxy::refuses_images(state, model_id)
}

/// `body` with every media part replaced by text, for the upstream `path`. A body with no media
/// part, or one that is not JSON, comes back unchanged. Never fails: a part that cannot be
/// converted becomes a short placeholder, and the hub logs a warning.
pub(crate) async fn convert(state: &AppState, path: &str, body: Bytes) -> Bytes {
    let Some(shape) = BodyShape::from_path(path) else {
        return body;
    };
    let Ok(mut json) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let items = collect_media(shape, &json);
    if items.is_empty() {
        return body;
    }
    let config = state.config();
    let texts = Converter {
        state,
        config: &config.media_fallback,
    }
    .texts(&items)
    .await;
    replace_media(shape, &mut json, &texts);
    serde_json::to_vec(&json).map_or(body, Bytes::from)
}

/// The work one media item needs.
enum Plan {
    /// The text is known without a call.
    Ready(String),
    /// The text comes from the cache or from `job`.
    Convert { key: String, job: Job },
}

/// One conversion that is not cached.
#[derive(Clone)]
enum Job {
    Caption {
        model: ModelId,
        data_uri: String,
    },
    Transcribe {
        model: ModelId,
        data: String,
        format: String,
    },
    PdfText {
        data: String,
    },
}

struct Converter<'a> {
    state: &'a AppState,
    config: &'a MediaFallbackConfig,
}

impl Converter<'_> {
    /// The replacement text of each item, in order.
    async fn texts(&self, items: &[MediaItem]) -> Vec<String> {
        let plans: Vec<Plan> = items.iter().map(|item| self.plan(item)).collect();
        let found = self.lookup(&plans).await;
        plans
            .into_iter()
            .zip(items)
            .map(|(plan, item)| match plan {
                Plan::Ready(text) => text,
                Plan::Convert { key, .. } => match found.get(&key) {
                    Some(Ok(text)) => converted(item, text),
                    Some(Err(reason)) => omitted(item.kind, reason),
                    None => omitted(item.kind, "not converted"),
                },
            })
            .collect()
    }

    fn plan(&self, item: &MediaItem) -> Plan {
        let kind = item.kind;
        match (&item.source, kind) {
            (MediaSource::Url(url), MediaKind::Image | MediaKind::Document) => {
                Plan::Ready(format!("[{}: {url}]", kind.as_str()))
            }
            (MediaSource::Text(text), MediaKind::Document) => {
                Plan::Ready(converted(item, &truncated(text)))
            }
            (MediaSource::Inline { data, .. }, _) if data.len() > self.config.max_item_bytes => {
                Plan::Ready(omitted(
                    kind,
                    &format!("over {} bytes", self.config.max_item_bytes),
                ))
            }
            (MediaSource::Inline { media_type, data }, _) => {
                self.inline_plan(item, media_type, data)
            }
            _ => Plan::Ready(omitted(kind, "file references are not supported")),
        }
    }

    fn inline_plan(&self, item: &MediaItem, media_type: &str, data: &str) -> Plan {
        let kind = item.kind;
        let job = match kind {
            MediaKind::Image if !IMAGE_TYPES.contains(&media_type) => {
                return Plan::Ready(omitted(kind, &format!("unsupported type {media_type}")));
            }
            MediaKind::Image => match &self.config.vision_model {
                Some(model) => Job::Caption {
                    model: model.clone(),
                    data_uri: format!("data:{media_type};base64,{data}"),
                },
                None => return Plan::Ready(omitted(kind, "no vision model is set")),
            },
            MediaKind::Audio => match &self.config.stt_model {
                Some(model) => Job::Transcribe {
                    model: model.clone(),
                    data: data.to_owned(),
                    format: media_type.trim_start_matches("audio/").to_owned(),
                },
                None => return Plan::Ready(omitted(kind, "no speech model is set")),
            },
            MediaKind::Document if media_type.starts_with("text/") => {
                return Plan::Ready(match decode_text(data) {
                    Ok(text) => converted(item, &truncated(&text)),
                    Err(reason) => omitted(kind, &reason),
                });
            }
            MediaKind::Document if media_type == "application/pdf" => Job::PdfText {
                data: data.to_owned(),
            },
            MediaKind::Document => {
                return Plan::Ready(omitted(kind, &format!("unsupported type {media_type}")));
            }
        };
        Plan::Convert {
            key: media_key(kind, &job.model(), data),
            job,
        }
    }

    /// The text or the failure of every key the plans name: from memory, then the store, then a
    /// conversion. At most `max_items` conversions run, all at once.
    async fn lookup(&self, plans: &[Plan]) -> HashMap<String, Result<String, String>> {
        let cache = self.state.media_cache();
        let mut found = HashMap::new();
        let mut missing: Vec<(&String, &Job)> = Vec::new();
        let mut seen = HashSet::new();
        for plan in plans {
            if let Plan::Convert { key, job } = plan
                && seen.insert(key)
            {
                match cache.get(key) {
                    Some(text) => {
                        found.insert(key.clone(), Ok(text));
                    }
                    None => missing.push((key, job)),
                }
            }
        }
        if missing.is_empty() {
            return found;
        }
        for (key, text) in self.stored(&missing).await {
            cache.insert(key.clone(), text.clone());
            found.insert(key, Ok(text));
        }
        missing.retain(|(key, _)| !found.contains_key(*key));
        let limit = self.config.max_items;
        for (key, _) in missing.iter().skip(limit) {
            found.insert(
                (*key).clone(),
                Err(format!("over the limit of {limit} per request")),
            );
        }
        let runs = missing
            .iter()
            .take(limit)
            .map(|(key, job)| async move { ((*key).clone(), (*job).clone(), self.run(job).await) });
        for (key, job, result) in join_all(runs).await {
            match &result {
                Ok(text) => self.remember(&key, &job, text),
                Err(reason) => tracing::warn!(
                    kind = job.kind().as_str(),
                    "media fallback failed: {reason}"
                ),
            }
            found.insert(key, result);
        }
        found
    }

    /// The saved texts of the `missing` keys. Reads the store off the runtime threads.
    async fn stored(&self, missing: &[(&String, &Job)]) -> HashMap<String, String> {
        if self.state.store().is_none() {
            return HashMap::new();
        }
        let keys: Vec<String> = missing.iter().map(|(key, _)| (*key).clone()).collect();
        let state = self.state.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            state
                .store()
                .map_or_else(|| Ok(HashMap::new()), |store| store.load_media_texts(&keys))
        })
        .await;
        match loaded {
            Ok(Ok(texts)) => texts,
            Ok(Err(e)) => {
                tracing::warn!("cannot read the saved media texts: {e}");
                HashMap::new()
            }
            Err(e) => {
                tracing::warn!("media text lookup task failed: {e}");
                HashMap::new()
            }
        }
    }

    async fn run(&self, job: &Job) -> Result<String, String> {
        let timeout = self.config.timeout;
        let work = async {
            match job {
                Job::Caption { model, data_uri } => {
                    media_calls::caption(self.state, model, data_uri, timeout).await
                }
                Job::Transcribe {
                    model,
                    data,
                    format,
                } => media_calls::transcribe(self.state, model, data, format, timeout).await,
                Job::PdfText { data } => {
                    let data = data.clone();
                    tokio::task::spawn_blocking(move || {
                        let pdf = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(|e| format!("the PDF is not base64: {e}"))?;
                        pdf_text::pdf_text(&pdf)
                    })
                    .await
                    .map_err(|e| format!("PDF task failed: {e}"))?
                }
            }
        };
        tokio::time::timeout(timeout, work)
            .await
            .unwrap_or_else(|_| Err("timeout".to_owned()))
    }

    /// Keeps a new text in memory, and saves it to the store off the calling task.
    fn remember(&self, key: &str, job: &Job, text: &str) {
        self.state
            .media_cache()
            .insert(key.to_owned(), text.to_owned());
        if self.state.store().is_none() {
            return;
        }
        let state = self.state.clone();
        let (key, text) = (key.to_owned(), text.to_owned());
        let (kind, model) = (job.kind(), job.model());
        tokio::task::spawn_blocking(move || {
            if let Some(store) = state.store()
                && let Err(e) = store.save_media_text(&key, kind.as_str(), &model, &text, now_ms())
            {
                tracing::warn!("cannot save the media text: {e}");
            }
        });
    }
}

impl Job {
    fn kind(&self) -> MediaKind {
        match self {
            Job::Caption { .. } => MediaKind::Image,
            Job::Transcribe { .. } => MediaKind::Audio,
            Job::PdfText { .. } => MediaKind::Document,
        }
    }

    fn model(&self) -> String {
        match self {
            Job::Caption { model, .. } | Job::Transcribe { model, .. } => model.qualified(),
            Job::PdfText { .. } => PDF_EXTRACTOR.to_owned(),
        }
    }
}

/// The text part that stands in for `item`.
fn converted(item: &MediaItem, text: &str) -> String {
    match (item.kind, &item.filename) {
        (MediaKind::Image, _) => format!("[image: {text}]"),
        (MediaKind::Audio, _) => format!("[audio transcript: {text}]"),
        (MediaKind::Document, Some(name)) => format!("[document {name}: {text}]"),
        (MediaKind::Document, None) => format!("[document: {text}]"),
    }
}

/// The placeholder of a part that was not converted.
fn omitted(kind: MediaKind, reason: &str) -> String {
    format!("[{} omitted: {reason}]", kind.as_str())
}

fn decode_text(data: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("the document is not base64: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "the document is not UTF-8 text".to_owned())
}

fn truncated(text: &str) -> String {
    match text.char_indices().nth(MAX_DOCUMENT_CHARS) {
        Some((cut, _)) => format!("{} [truncated]", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{HeaderMap, Method};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::model_descriptor::ModelDescriptor;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::store::{StatsFilter, Store};
    use crate::services::transforms::RouteKind;

    const CHAT: &str = "/v1/chat/completions";
    const PNG: &str = "data:image/png;base64,iVBORw0KGgo=";

    fn state_with(server: &MockServer, extra: &[(&str, &str)], store: Option<Store>) -> AppState {
        let uri = server.uri();
        let mut vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "local,vision,stt".to_owned()),
            ("LLM_HUB_LOCAL_BASE_URL", format!("{uri}/local")),
            ("LLM_HUB_VISION_BASE_URL", format!("{uri}/vision")),
            ("LLM_HUB_STT_BASE_URL", format!("{uri}/stt")),
            ("LLM_HUB_MEDIA_FALLBACK", "true".to_owned()),
            (
                "LLM_HUB_MEDIA_FALLBACK_VISION_MODEL",
                "vision/eye".to_owned(),
            ),
            ("LLM_HUB_MEDIA_FALLBACK_STT_MODEL", "stt/ear".to_owned()),
            ("LLM_HUB_MEDIA_FALLBACK_TIMEOUT_MS", "2000".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        for (key, value) in extra {
            vars.insert(format!("LLM_HUB_{key}"), (*value).to_owned());
        }
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn completion(text: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "c1",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}
        }))
    }

    async fn mount_caption(server: &MockServer, text: &str, calls: u64) {
        Mock::given(method("POST"))
            .and(path("/vision/chat/completions"))
            .and(body_string_contains("Describe this image"))
            .respond_with(completion(text))
            .expect(calls)
            .mount(server)
            .await;
    }

    fn chat_body(parts: Value) -> Value {
        let mut body = json!({"model": "local/text", "messages": [
            {"role": "user", "content": "look"},
            {"role": "tool", "tool_call_id": "c1", "content": null}
        ]});
        body["messages"][1]["content"] = parts;
        body
    }

    async fn converted_parts(state: &AppState, parts: Value) -> Value {
        let body = Bytes::from(chat_body(parts).to_string());
        let out = convert(state, "/chat/completions", body).await;
        serde_json::from_slice::<Value>(&out).unwrap()["messages"][1]["content"].clone()
    }

    fn image(url: &str) -> Value {
        json!({"type": "image_url", "image_url": {"url": url}})
    }

    #[tokio::test]
    async fn an_image_becomes_its_caption_once_and_the_call_is_logged() {
        let server = MockServer::start().await;
        mount_caption(&server, " a red square ", 1).await;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let store = Store::open("sqlite", db.to_str()).unwrap();
        let state = state_with(&server, &[], Some(store));

        let parts = converted_parts(&state, json!([image(PNG), image(PNG)])).await;

        let caption = json!({"type": "text", "text": "[image: a red square]"});
        assert_eq!(parts, json!([caption.clone(), caption.clone()]));
        let again = converted_parts(&state, json!([image(PNG)])).await;
        assert_eq!(again, json!([caption]));
        let outcomes = state
            .store()
            .unwrap()
            .filtered_outcomes(&StatsFilter::default())
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].client.as_deref(),
            Some("llm-hub/media-fallback")
        );
        assert_eq!(outcomes[0].model_key, "vision/eye");
    }

    #[tokio::test]
    async fn a_saved_caption_survives_a_restart_without_a_call() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let key = media_key(MediaKind::Image, "vision/eye", "iVBORw0KGgo=");
        Store::open("sqlite", db.to_str())
            .unwrap()
            .save_media_text(&key, "image", "vision/eye", "saved", 1)
            .unwrap();
        let server = MockServer::start().await;
        mount_caption(&server, "fresh", 0).await;
        let state = state_with(&server, &[], Store::open("sqlite", db.to_str()).ok());

        let parts = converted_parts(&state, json!([image(PNG)])).await;

        assert_eq!(parts[0]["text"], "[image: saved]");
        assert_eq!(state.media_cache().get(&key).as_deref(), Some("saved"));
    }

    #[tokio::test]
    async fn a_new_caption_is_saved_to_the_store() {
        let server = MockServer::start().await;
        mount_caption(&server, "a cat", 1).await;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let state = state_with(&server, &[], Store::open("json", db.to_str()).ok());

        converted_parts(&state, json!([image(PNG)])).await;

        let key = media_key(MediaKind::Image, "vision/eye", "iVBORw0KGgo=");
        for _ in 0..50 {
            let saved = state
                .store()
                .unwrap()
                .load_media_texts(std::slice::from_ref(&key))
                .unwrap();
            if !saved.is_empty() {
                assert_eq!(saved[&key], "a cat");
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the caption was never saved");
    }

    #[tokio::test]
    async fn failures_and_limits_become_placeholders() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/vision/chat/completions"))
            .and(body_string_contains("AAAA"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/vision/chat/completions"))
            .and(body_string_contains("BBBB"))
            .respond_with(completion("x").set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/vision/chat/completions"))
            .and(body_string_contains("CCCC"))
            .respond_with(completion("   "))
            .mount(&server)
            .await;
        let state = state_with(
            &server,
            &[
                ("MEDIA_FALLBACK_MAX_ITEMS", "3"),
                ("MEDIA_FALLBACK_MAX_ITEM_BYTES", "64"),
                ("MEDIA_FALLBACK_TIMEOUT_MS", "300"),
            ],
            None,
        );

        let parts = converted_parts(
            &state,
            json!([
                image("data:image/png;base64,AAAA"),
                image("data:image/png;base64,BBBB"),
                image("data:image/png;base64,CCCC"),
                image("data:image/png;base64,DDDD"),
                image(&format!("data:image/png;base64,{}", "E".repeat(65))),
                image("data:image/svg+xml;base64,FFFF"),
                image("https://example.com/cat.png"),
                {"type": "image_url"},
            ]),
        )
        .await;

        let texts: Vec<&str> = parts
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "[image omitted: HTTP 500]",
                "[image omitted: timeout]",
                "[image omitted: the caption is empty]",
                "[image omitted: over the limit of 3 per request]",
                "[image omitted: over 64 bytes]",
                "[image omitted: unsupported type image/svg+xml]",
                "[image: https://example.com/cat.png]",
                "[image omitted: file references are not supported]",
            ]
        );
    }

    #[tokio::test]
    async fn audio_becomes_its_transcript() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/stt/audio/transcriptions"))
            .and(body_string_contains("filename=\"audio.mp3\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "hello there"})))
            .expect(1)
            .mount(&server)
            .await;
        let state = state_with(&server, &[], None);

        let parts = converted_parts(
            &state,
            json!([
                {"type": "input_audio", "input_audio": {"data": "AAEC", "format": "mp3"}},
                {"type": "input_audio", "input_audio": {"data": "not base64!", "format": "wav"}}
            ]),
        )
        .await;

        assert_eq!(parts[0]["text"], "[audio transcript: hello there]");
        let failed = parts[1]["text"].as_str().unwrap();
        assert!(
            failed.starts_with("[audio omitted: the audio is not base64"),
            "{failed}"
        );
    }

    #[tokio::test]
    async fn documents_become_their_text() {
        let server = MockServer::start().await;
        let state = state_with(&server, &[], None);
        let long = "y".repeat(MAX_DOCUMENT_CHARS + 5);

        let parts = converted_parts(
            &state,
            json!([
                {"type": "file", "file": {"file_data": "data:text/plain;base64,aGk=", "filename": "a.txt"}},
                {"type": "file", "file": {"file_data": "data:text/plain;base64,//8=", "filename": "b.txt"}},
                {"type": "file", "file": {"file_data": "data:text/plain;base64,***"}},
                {"type": "file", "file": {"file_data": "data:application/zip;base64,AAAA"}},
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,***"}},
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,JVBERi0xLjQ="}},
                {"type": "file", "file": {"file_data": format!("data:text/plain;base64,{}", base64::engine::general_purpose::STANDARD.encode(&long))}}
            ]),
        )
        .await;

        assert_eq!(parts[0]["text"], "[document a.txt: hi]");
        assert_eq!(
            parts[1]["text"],
            "[document omitted: the document is not UTF-8 text]"
        );
        assert!(
            parts[2]["text"]
                .as_str()
                .unwrap()
                .starts_with("[document omitted: the document is not base64")
        );
        assert_eq!(
            parts[3]["text"],
            "[document omitted: unsupported type application/zip]"
        );
        assert!(
            parts[4]["text"]
                .as_str()
                .unwrap()
                .starts_with("[document omitted: the PDF is not base64")
        );
        assert!(
            parts[5]["text"]
                .as_str()
                .unwrap()
                .starts_with("[document omitted: ")
        );
        assert!(
            parts[6]["text"]
                .as_str()
                .unwrap()
                .ends_with("y [truncated]]")
        );
    }

    #[tokio::test]
    async fn without_models_media_becomes_placeholders() {
        let server = MockServer::start().await;
        let state = state_with(
            &server,
            &[
                ("MEDIA_FALLBACK_VISION_MODEL", ""),
                ("MEDIA_FALLBACK_STT_MODEL", ""),
            ],
            None,
        );

        let parts = converted_parts(
            &state,
            json!([
                image(PNG),
                {"type": "input_audio", "input_audio": {"data": "AAEC", "format": "mp3"}}
            ]),
        )
        .await;

        assert_eq!(parts[0]["text"], "[image omitted: no vision model is set]");
        assert_eq!(parts[1]["text"], "[audio omitted: no speech model is set]");
    }

    #[tokio::test]
    async fn bodies_with_nothing_to_convert_come_back_unchanged() {
        let server = MockServer::start().await;
        let state = state_with(&server, &[], None);
        for (path, body) in [
            ("/embeddings", Bytes::from_static(b"{}")),
            (CHAT, Bytes::from_static(b"not json")),
            (CHAT, Bytes::from(chat_body(json!("text")).to_string())),
        ] {
            assert_eq!(convert(&state, path, body.clone()).await, body);
        }
    }

    #[tokio::test]
    async fn text_only_targets_come_from_globs_descriptors_and_refusals() {
        let server = MockServer::start().await;
        let state = state_with(&server, &[("MEDIA_FALLBACK_TEXT_ONLY", "local/q*")], None);
        let config = state.config();
        let model = |raw: &str| ModelId::parse(raw).unwrap();
        state.set_model_descriptors(vec![ModelDescriptor {
            vision: Some(false),
            ..ModelDescriptor::for_test("local/blind")
        }]);
        state.learn_vision_unsupported("local", "learned");

        let text_only = |raw: &str| targets_text_only(&state, &config.media_fallback, &model(raw));
        assert!(text_only("local/qwen"));
        assert!(text_only("local/blind"));
        assert!(text_only("local/learned"));
        assert!(!text_only("local/other"));
        assert!(request_has_media(CHAT, &chat_body(json!([image(PNG)]))));
        assert!(!request_has_media(
            "/embeddings",
            &chat_body(json!([image(PNG)]))
        ));
    }

    async fn send(state: &AppState, body: &Value) -> (u16, Value) {
        let response = proxy::attempt_loop(
            state,
            Method::POST,
            CHAT,
            &HeaderMap::new(),
            Bytes::from(body.to_string()),
            RouteKind::OpenAi,
        )
        .await
        .unwrap();
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn local_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/local/chat/completions")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_text_only_model_gets_captions_and_others_get_the_image() {
        let server = MockServer::start().await;
        mount_caption(&server, "a cat", 1).await;
        Mock::given(method("POST"))
            .and(path("/local/chat/completions"))
            .respond_with(completion("ok"))
            .mount(&server)
            .await;
        let state = state_with(&server, &[], None);
        state.set_model_descriptors(vec![ModelDescriptor {
            vision: Some(false),
            ..ModelDescriptor::for_test("local/text")
        }]);

        let (status, _) = send(&state, &chat_body(json!([image(PNG)]))).await;
        let mut vision = chat_body(json!([image(PNG)]));
        vision["model"] = json!("local/sees");
        let (vision_status, _) = send(&state, &vision).await;

        assert_eq!((status, vision_status), (200, 200));
        let bodies = local_bodies(&server).await;
        assert_eq!(
            bodies[0]["messages"][1]["content"],
            json!([{"type": "text", "text": "[image: a cat]"}])
        );
        assert_eq!(bodies[1]["messages"][1]["content"], json!([image(PNG)]));
    }

    #[tokio::test]
    async fn an_image_refusal_teaches_the_model_and_the_attempt_is_sent_again_as_text() {
        let server = MockServer::start().await;
        mount_caption(&server, "a cat", 1).await;
        Mock::given(method("POST"))
            .and(path("/local/chat/completions"))
            .and(body_string_contains("image_url"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string("image input is not supported"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/local/chat/completions"))
            .respond_with(completion("ok"))
            .mount(&server)
            .await;
        let state = state_with(&server, &[], None);

        let (status, reply) = send(&state, &chat_body(json!([image(PNG)]))).await;

        assert_eq!(status, 200);
        assert_eq!(reply["choices"][0]["message"]["content"], "ok");
        assert!(state.vision_unsupported("local", "text"));
        let bodies = local_bodies(&server).await;
        assert_eq!(
            bodies[1]["messages"][1]["content"][0]["text"],
            "[image: a cat]"
        );
    }

    #[tokio::test]
    async fn another_500_reaches_the_client_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/local/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
            .expect(1)
            .mount(&server)
            .await;
        let state = state_with(&server, &[], None);

        let (status, reply) = send(&state, &chat_body(json!([image(PNG)]))).await;

        assert_eq!(status, 500);
        assert_eq!(reply, json!({"error": "boom"}));
        assert!(!state.vision_unsupported("local", "text"));
    }

    #[tokio::test]
    async fn when_off_a_known_text_only_model_is_still_refused() {
        let server = MockServer::start().await;
        let state = state_with(&server, &[("MEDIA_FALLBACK", "false")], None);
        state.learn_vision_unsupported("local", "text");

        let error = proxy::attempt_loop(
            &state,
            Method::POST,
            CHAT,
            &HeaderMap::new(),
            Bytes::from(chat_body(json!([image(PNG)])).to_string()),
            RouteKind::OpenAi,
        )
        .await
        .unwrap_err();

        assert!(
            format!("{error:?}").contains("image input is not supported"),
            "{error:?}"
        );
    }
}

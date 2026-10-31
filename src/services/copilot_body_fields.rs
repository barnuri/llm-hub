//! Anthropic request fields that Copilot's `/v1/messages` rejects with
//! "<path>: Extra inputs are not permitted". The hub learns each one from the
//! rejection and strips it from later requests to the same profile.

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use serde_json::Value;

use crate::services::store::Store;

/// A request field Copilot does not accept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BodyField {
    /// A top-level key, such as `context_management`.
    TopLevel(String),
    /// A key on every message, learned from a `messages.<N>.<key>` path.
    EachMessage(String),
}

impl BodyField {
    const MESSAGES_KEY: &str = "messages";
    /// Stands for "every index" in the stored form of [`BodyField::EachMessage`].
    const ANY_INDEX: &str = "*";
    const EXTRA_INPUT_SUFFIX: &str = ": Extra inputs are not permitted";

    /// Every field a Copilot rejection body names as an extra input. Paths of
    /// any other shape are ignored.
    pub fn rejected_in(error_body: &[u8]) -> Vec<BodyField> {
        let Ok(body) = serde_json::from_slice::<Value>(error_body) else {
            return Vec::new();
        };
        let message = body
            .pointer("/error/message")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        message
            .lines()
            .filter_map(|line| line.trim().strip_suffix(Self::EXTRA_INPUT_SUFFIX))
            .filter_map(|path| Self::from_path(path, |index| index.parse::<usize>().is_ok()))
            .collect()
    }

    /// Parses the stored form written by [`BodyField::stored`].
    pub fn from_stored(stored: &str) -> Option<BodyField> {
        Self::from_path(stored, |index| index == Self::ANY_INDEX)
    }

    /// `context_management`, or `messages.*.output_config`.
    pub fn stored(&self) -> String {
        match self {
            BodyField::TopLevel(key) => key.clone(),
            BodyField::EachMessage(key) => {
                format!("{}.{}.{key}", Self::MESSAGES_KEY, Self::ANY_INDEX)
            }
        }
    }

    fn from_path(path: &str, is_index: impl Fn(&str) -> bool) -> Option<BodyField> {
        let segments: Vec<&str> = path.split('.').collect();
        match segments.as_slice() {
            [key] if Self::is_key(key) => Some(BodyField::TopLevel((*key).to_string())),
            [messages, index, key]
                if *messages == Self::MESSAGES_KEY && is_index(index) && Self::is_key(key) =>
            {
                Some(BodyField::EachMessage((*key).to_string()))
            }
            _ => None,
        }
    }

    fn is_key(key: &str) -> bool {
        !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    /// Removes the field from `body`.
    pub fn strip(&self, body: &mut Value) {
        match self {
            BodyField::TopLevel(key) => {
                if let Some(object) = body.as_object_mut() {
                    object.remove(key);
                }
            }
            BodyField::EachMessage(key) => {
                let messages = body
                    .get_mut(Self::MESSAGES_KEY)
                    .and_then(Value::as_array_mut);
                for message in messages.into_iter().flatten() {
                    if let Some(object) = message.as_object_mut() {
                        object.remove(key);
                    }
                }
            }
        }
    }
}

/// Profile name -> the fields its Copilot upstream rejected, read on the request path.
#[derive(Debug, Default)]
pub struct UnsupportedBodyFields {
    table: DashMap<String, Arc<Vec<BodyField>>>,
}

impl UnsupportedBodyFields {
    /// Loads every saved field. Call at startup only: it reads the store.
    pub fn load(store: Option<&Store>) -> UnsupportedBodyFields {
        let stored = store.map_or_else(
            || Ok(HashMap::new()),
            |store| {
                store.load_unsupported_body_fields().inspect_err(|e| {
                    tracing::warn!("cannot load the unsupported body fields: {e}");
                })
            },
        );
        let table = DashMap::new();
        for (profile, fields) in stored.unwrap_or_default() {
            let fields: Vec<BodyField> = fields
                .iter()
                .filter_map(|stored| BodyField::from_stored(stored))
                .collect();
            table.insert(profile, Arc::new(fields));
        }
        UnsupportedBodyFields { table }
    }

    /// Empty when `profile` never rejected a field.
    pub fn for_profile(&self, profile: &str) -> Arc<Vec<BodyField>> {
        self.table
            .get(profile)
            .map(|fields| Arc::clone(&fields))
            .unwrap_or_default()
    }

    /// Records `fields` for `profile` and returns the ones it did not know yet.
    pub fn learn(&self, profile: &str, fields: &[BodyField]) -> Vec<BodyField> {
        let mut entry = self.table.entry(profile.to_string()).or_default();
        let new: Vec<BodyField> = fields
            .iter()
            .filter(|field| !entry.contains(field))
            .cloned()
            .collect();
        if !new.is_empty() {
            let mut merged = (**entry).clone();
            merged.extend(new.iter().cloned());
            *entry = Arc::new(merged);
        }
        new
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rejection(message: &str) -> Vec<u8> {
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn rejection_names_a_top_level_field_and_a_message_field() {
        let body = rejection(
            "context_management: Extra inputs are not permitted\nmessages.1.output_config: Extra inputs are not permitted",
        );

        assert_eq!(
            BodyField::rejected_in(&body),
            vec![
                BodyField::TopLevel("context_management".to_string()),
                BodyField::EachMessage("output_config".to_string()),
            ]
        );
    }

    #[test]
    fn other_rejections_and_other_paths_name_no_field() {
        let other = rejection("thinking is not supported");
        let nested = rejection("messages.1.content.0.citations: Extra inputs are not permitted");
        let tools = rejection("tools.0.defer_loading: Extra inputs are not permitted");

        assert!(BodyField::rejected_in(&other).is_empty());
        assert!(BodyField::rejected_in(&nested).is_empty());
        assert!(BodyField::rejected_in(&tools).is_empty());
        assert!(BodyField::rejected_in(b"not json").is_empty());
    }

    #[test]
    fn stored_form_round_trips() {
        for field in [
            BodyField::TopLevel("context_management".to_string()),
            BodyField::EachMessage("output_config".to_string()),
        ] {
            assert_eq!(BodyField::from_stored(&field.stored()), Some(field));
        }
        assert_eq!(BodyField::from_stored("messages.1.output_config"), None);
    }

    #[test]
    fn strip_removes_the_key_from_the_body_and_every_message() {
        let mut body = json!({
            "context_management": {"edits": []},
            "messages": [
                {"role": "user", "content": "hi", "output_config": {}},
                {"role": "assistant", "content": "yo", "output_config": {}},
            ],
        });

        BodyField::TopLevel("context_management".to_string()).strip(&mut body);
        BodyField::EachMessage("output_config".to_string()).strip(&mut body);

        assert_eq!(
            body,
            json!({"messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "yo"},
            ]})
        );
    }

    #[test]
    fn learn_returns_only_new_fields_per_profile() {
        let fields = UnsupportedBodyFields::default();
        let context = BodyField::TopLevel("context_management".to_string());

        let once = std::slice::from_ref(&context);

        assert_eq!(fields.learn("a", once), once);
        assert!(fields.learn("a", once).is_empty());
        assert_eq!(*fields.for_profile("a"), once);
        assert!(fields.for_profile("b").is_empty());
    }

    #[test]
    fn load_reads_the_fields_saved_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        store
            .save_unsupported_body_field("copilot", "messages.*.output_config", 1)
            .unwrap();
        store
            .save_unsupported_body_field("copilot", "messages.1.unreadable", 2)
            .unwrap();

        let fields = UnsupportedBodyFields::load(Some(&store));

        assert_eq!(
            *fields.for_profile("copilot"),
            [BodyField::EachMessage("output_config".to_string())]
        );
    }
}

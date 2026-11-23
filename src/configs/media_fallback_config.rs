use std::time::Duration;

use crate::consts::{
    DEFAULT_MEDIA_FALLBACK_MAX_ITEM_BYTES, DEFAULT_MEDIA_FALLBACK_MAX_ITEMS,
    DEFAULT_MEDIA_FALLBACK_TIMEOUT_MS, DEFAULT_MEDIA_FALLBACK_VISION_MODEL,
};
use crate::schemas::model_id::ModelId;

/// Media fallback settings from the `LLM_HUB_MEDIA_FALLBACK*` keys. Off by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFallbackConfig {
    /// `LLM_HUB_MEDIA_FALLBACK`: convert media parts to text for text-only models.
    pub enabled: bool,
    /// `LLM_HUB_MEDIA_SKIP`: drop media parts a text-only model cannot read and leave a system
    /// reminder in their place. With conversion on too, only the parts no model converts are dropped.
    pub skip: bool,
    /// `LLM_HUB_MEDIA_FALLBACK_VISION_MODEL`: the model that captions images. `None` skips images.
    pub vision_model: Option<ModelId>,
    /// `LLM_HUB_MEDIA_FALLBACK_STT_MODEL`: the model that transcribes audio. `None` skips audio.
    pub stt_model: Option<ModelId>,
    /// `LLM_HUB_MEDIA_FALLBACK_TEXT_ONLY`: model globs that always get text, on top of the known ones.
    pub text_only: Vec<String>,
    /// `LLM_HUB_MEDIA_FALLBACK_MAX_ITEM_BYTES`: a larger media item becomes a placeholder.
    pub max_item_bytes: usize,
    /// `LLM_HUB_MEDIA_FALLBACK_MAX_ITEMS`: model calls per request; later items become placeholders.
    pub max_items: usize,
    /// `LLM_HUB_MEDIA_FALLBACK_TIMEOUT_MS`: the longest wait for one conversion.
    pub timeout: Duration,
}

impl Default for MediaFallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            skip: false,
            vision_model: ModelId::parse(DEFAULT_MEDIA_FALLBACK_VISION_MODEL),
            stt_model: None,
            text_only: Vec::new(),
            max_item_bytes: DEFAULT_MEDIA_FALLBACK_MAX_ITEM_BYTES,
            max_items: DEFAULT_MEDIA_FALLBACK_MAX_ITEMS,
            timeout: Duration::from_millis(DEFAULT_MEDIA_FALLBACK_TIMEOUT_MS),
        }
    }
}

impl MediaFallbackConfig {
    /// Whether the hub touches media parts at all: to convert them, to drop them, or both.
    #[must_use]
    pub fn active(&self) -> bool {
        self.enabled || self.skip
    }

    /// Reads the settings through `get`, which takes a key without the `LLM_HUB_` prefix.
    ///
    /// # Errors
    /// Returns a message naming the key when a model id or a number does not parse.
    pub fn from_get(
        get: &impl Fn(&str) -> Option<String>,
        is_truthy: impl Fn(&str) -> bool,
    ) -> Result<Self, String> {
        let defaults = Self::default();
        let model = |key: &str, default: Option<ModelId>| match get(key) {
            None => Ok(default),
            Some(v) if v.is_empty() => Ok(None),
            Some(v) => ModelId::parse(&v)
                .map(Some)
                .ok_or_else(|| format!("LLM_HUB_{key} must be <profile>/<model>: {v}")),
        };
        let number = |key: &str, default: u64| match get(key).filter(|v| !v.is_empty()) {
            None => Ok(default),
            Some(v) => v
                .parse::<u64>()
                .map_err(|_| format!("LLM_HUB_{key} is not a number: {v}")),
        };
        let to_usize = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
        Ok(Self {
            enabled: get("MEDIA_FALLBACK").is_some_and(|v| is_truthy(&v)),
            skip: get("MEDIA_SKIP").is_some_and(|v| is_truthy(&v)),
            vision_model: model("MEDIA_FALLBACK_VISION_MODEL", defaults.vision_model)?,
            stt_model: model("MEDIA_FALLBACK_STT_MODEL", None)?,
            text_only: get("MEDIA_FALLBACK_TEXT_ONLY")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|glob| !glob.is_empty())
                .map(str::to_owned)
                .collect(),
            max_item_bytes: to_usize(number(
                "MEDIA_FALLBACK_MAX_ITEM_BYTES",
                defaults.max_item_bytes as u64,
            )?),
            max_items: to_usize(number(
                "MEDIA_FALLBACK_MAX_ITEMS",
                defaults.max_items as u64,
            )?),
            timeout: Duration::from_millis(number(
                "MEDIA_FALLBACK_TIMEOUT_MS",
                DEFAULT_MEDIA_FALLBACK_TIMEOUT_MS,
            )?),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<MediaFallbackConfig, String> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        MediaFallbackConfig::from_get(&|key: &str| vars.get(key).cloned(), |v| v == "true")
    }

    #[test]
    fn defaults_are_off_with_the_local_vision_model_and_no_speech_model() {
        let config = parse(&[]).unwrap();
        assert_eq!(config, MediaFallbackConfig::default());
        assert!(!config.enabled);
        assert!(!config.skip);
        assert!(!config.active());
        assert_eq!(
            config.vision_model.unwrap().qualified(),
            DEFAULT_MEDIA_FALLBACK_VISION_MODEL
        );
        assert!(config.stt_model.is_none());
        assert!(config.text_only.is_empty());
    }

    #[test]
    fn every_key_overrides_its_default() {
        let config = parse(&[
            ("MEDIA_FALLBACK", "true"),
            ("MEDIA_SKIP", "true"),
            ("MEDIA_FALLBACK_VISION_MODEL", "local/vision"),
            ("MEDIA_FALLBACK_STT_MODEL", "whisper/large"),
            ("MEDIA_FALLBACK_TEXT_ONLY", "local/*, ,other/m"),
            ("MEDIA_FALLBACK_MAX_ITEM_BYTES", "100"),
            ("MEDIA_FALLBACK_MAX_ITEMS", "2"),
            ("MEDIA_FALLBACK_TIMEOUT_MS", "500"),
        ])
        .unwrap();
        assert!(config.enabled);
        assert!(config.skip);
        assert_eq!(config.vision_model.unwrap().qualified(), "local/vision");
        assert_eq!(config.stt_model.unwrap().qualified(), "whisper/large");
        assert_eq!(config.text_only, ["local/*", "other/m"]);
        assert_eq!(config.max_item_bytes, 100);
        assert_eq!(config.max_items, 2);
        assert_eq!(config.timeout, Duration::from_millis(500));
    }

    #[test]
    fn an_empty_vision_model_turns_captions_off() {
        let config = parse(&[("MEDIA_FALLBACK_VISION_MODEL", "")]).unwrap();
        assert!(config.vision_model.is_none());
    }

    #[test]
    fn a_bad_model_or_number_names_its_key() {
        let err = parse(&[("MEDIA_FALLBACK_STT_MODEL", "no-slash")]).unwrap_err();
        assert!(err.contains("LLM_HUB_MEDIA_FALLBACK_STT_MODEL"), "{err}");
        let err = parse(&[("MEDIA_FALLBACK_MAX_ITEMS", "many")]).unwrap_err();
        assert!(err.contains("LLM_HUB_MEDIA_FALLBACK_MAX_ITEMS"), "{err}");
    }
}

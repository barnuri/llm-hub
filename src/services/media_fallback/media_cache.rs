//! The texts made from media, keyed by a hash of the media, the model and the kind. Memory holds
//! the most recently used ones; the store keeps every one, so a restart never converts again.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

use super::media_parts::MediaKind;

/// The cache key of `data` converted by `model` as `kind`: a SHA-256 hex digest.
pub(crate) fn media_key(kind: MediaKind, model: &str, data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(model.as_bytes());
    hasher.update([0]);
    hasher.update(data.as_bytes());
    crate::utils::hex::to_hex(&hasher.finalize())
}

/// A bounded least-recently-used map from media key to text.
#[derive(Debug)]
pub(crate) struct MediaCache {
    capacity: usize,
    entries: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    tick: u64,
    texts: HashMap<String, (String, u64)>,
}

impl MediaCache {
    pub(crate) fn new(capacity: usize) -> MediaCache {
        MediaCache {
            capacity: capacity.max(1),
            entries: Mutex::new(Entries::default()),
        }
    }

    /// The text for `key`, which becomes the most recently used.
    pub(crate) fn get(&self, key: &str) -> Option<String> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.tick += 1;
        let tick = entries.tick;
        let (text, used) = entries.texts.get_mut(key)?;
        *used = tick;
        Some(text.clone())
    }

    /// Keeps `text` for `key`, dropping the least recently used text when full.
    pub(crate) fn insert(&self, key: String, text: String) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.tick += 1;
        let tick = entries.tick;
        if !entries.texts.contains_key(&key)
            && entries.texts.len() >= self.capacity
            && let Some(oldest) = entries
                .texts
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
        {
            entries.texts.remove(&oldest);
        }
        entries.texts.insert(key, (text, tick));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_depends_on_kind_model_and_data() {
        let base = media_key(MediaKind::Image, "a/m", "AAAA");
        assert_eq!(base.len(), 64);
        assert_eq!(base, media_key(MediaKind::Image, "a/m", "AAAA"));
        assert_ne!(base, media_key(MediaKind::Audio, "a/m", "AAAA"));
        assert_ne!(base, media_key(MediaKind::Image, "a/n", "AAAA"));
        assert_ne!(base, media_key(MediaKind::Image, "a/m", "AAAB"));
    }

    #[test]
    fn a_full_cache_drops_the_least_recently_used_text() {
        let cache = MediaCache::new(2);
        cache.insert("a".into(), "A".into());
        cache.insert("b".into(), "B".into());
        assert_eq!(cache.get("a").as_deref(), Some("A"));
        cache.insert("c".into(), "C".into());

        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a").as_deref(), Some("A"));
        assert_eq!(cache.get("c").as_deref(), Some("C"));
    }

    #[test]
    fn replacing_a_key_keeps_the_others() {
        let cache = MediaCache::new(2);
        cache.insert("a".into(), "A".into());
        cache.insert("b".into(), "B".into());
        cache.insert("a".into(), "A2".into());
        assert_eq!(cache.get("a").as_deref(), Some("A2"));
        assert_eq!(cache.get("b").as_deref(), Some("B"));
    }
}

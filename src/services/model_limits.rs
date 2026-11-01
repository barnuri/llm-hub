//! The prompt limit each Copilot model enforces: seeded with measured values,
//! then learned from the upstream's own rejections, never probed.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::services::store::Store;

/// Where a limit came from. Stored as text, never as a database enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitSource {
    /// Measured by hand and shipped with the hub.
    Seeded,
    /// Read from an upstream `model_max_prompt_tokens_exceeded` rejection.
    Learned,
}

/// One model's prompt limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimit {
    /// Prompt tokens the upstream accepts.
    pub limit: u64,
    pub source: LimitSource,
    /// Unix milliseconds (UTC) when the limit was seeded or learned.
    pub learned_at: u64,
}

type ProfileLimits = HashMap<String, ModelLimit>;

/// Profile name -> upstream model id -> limit, read lock-free on the request path.
/// A profile with no entry of its own gets the seeded limits.
#[derive(Debug, Default)]
pub struct ModelLimits {
    table: ArcSwap<HashMap<String, Arc<ProfileLimits>>>,
    /// Serialises writers so a learned limit is compared and replaced in one step.
    writer: Mutex<()>,
}

impl LimitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitSource::Seeded => "seeded",
            LimitSource::Learned => "learned",
        }
    }

    pub fn parse(raw: &str) -> Option<LimitSource> {
        match raw {
            "seeded" => Some(LimitSource::Seeded),
            "learned" => Some(LimitSource::Learned),
            _ => None,
        }
    }
}

impl ModelLimits {
    /// The error code Copilot answers an oversized prompt with.
    const PROMPT_LIMIT_ERROR_CODE: &str = "model_max_prompt_tokens_exceeded";
    /// The word after the limit in "N tokens > M maximum".
    const LIMIT_MARKER: &str = "maximum";
    /// Copilot `/v1/messages` limits measured with oversized probes on 2026-10-31.
    pub const SEEDED: [(&str, u64); 8] = [
        ("claude-opus-5.5", 1_000_000),
        ("claude-opus-5", 1_000_000),
        ("claude-opus-4.8", 1_000_000),
        ("claude-opus-4.8-fast", 1_000_000),
        ("claude-sonnet-5.5", 1_000_000),
        ("claude-sonnet-5", 1_000_000),
        ("claude-haiku-5.5", 1_000_000),
        ("claude-haiku-4.5", 200_000),
    ];

    /// Seeds every Copilot profile (a seed never replaces a learned limit), then
    /// loads every saved limit. Call at startup only.
    pub fn load<'a>(
        store: Option<&Store>,
        copilot_profiles: impl IntoIterator<Item = &'a str>,
        now_ms: u64,
    ) -> ModelLimits {
        let mut table: HashMap<String, Arc<ProfileLimits>> = HashMap::new();
        for profile in copilot_profiles {
            if let Some(store) = store
                && let Err(e) = store.seed_model_limits(profile, &Self::SEEDED, now_ms)
            {
                tracing::warn!(profile, "cannot seed the model prompt limits: {e}");
            }
            table.insert(profile.to_string(), Arc::clone(&SEEDED_LIMITS));
        }
        let stored = store.map_or_else(
            || Ok(HashMap::new()),
            |store| {
                store
                    .load_model_limits()
                    .inspect_err(|e| tracing::warn!("cannot load the model prompt limits: {e}"))
            },
        );
        for (profile, limits) in stored.unwrap_or_default() {
            let mut merged = (**table.get(&profile).unwrap_or(&SEEDED_LIMITS)).clone();
            merged.extend(limits);
            table.insert(profile, Arc::new(merged));
        }
        ModelLimits {
            table: ArcSwap::from_pointee(table),
            writer: Mutex::new(()),
        }
    }

    /// The limits of `profile`'s models, keyed by upstream model id.
    pub fn for_profile(&self, profile: &str) -> Arc<ProfileLimits> {
        self.table
            .load()
            .get(profile)
            .map_or_else(|| Arc::clone(&SEEDED_LIMITS), Arc::clone)
    }

    /// Records `limit` for `model`. Returns the new entry and the limit it
    /// replaced, or `None` when the known limit is already `limit`.
    pub fn learn(
        &self,
        profile: &str,
        model: &str,
        limit: u64,
        now_ms: u64,
    ) -> Option<(ModelLimit, Option<u64>)> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let current = self.for_profile(profile);
        let previous = current.get(model).map(|entry| entry.limit);
        if previous == Some(limit) {
            return None;
        }
        let entry = ModelLimit {
            limit,
            source: LimitSource::Learned,
            learned_at: now_ms,
        };
        let mut limits = (*current).clone();
        limits.insert(model.to_string(), entry);
        let mut table = (**self.table.load()).clone();
        table.insert(profile.to_string(), Arc::new(limits));
        self.table.store(Arc::new(table));
        Some((entry, previous))
    }

    /// `M` from a Copilot 400 body whose `error.code` is
    /// `model_max_prompt_tokens_exceeded` and whose message ends "N tokens > M maximum".
    pub fn limit_from_error(body: &[u8]) -> Option<u64> {
        let value: Value = serde_json::from_slice(body).ok()?;
        let error = value.get("error")?;
        if error.get("code").and_then(Value::as_str) != Some(Self::PROMPT_LIMIT_ERROR_CODE) {
            return None;
        }
        let message = error.get("message").and_then(Value::as_str)?;
        let (_, after) = message.rsplit_once('>')?;
        let mut words = after.split_whitespace();
        let number = words.next()?;
        if words.next() != Some(Self::LIMIT_MARKER) {
            return None;
        }
        number
            .replace(',', "")
            .parse()
            .ok()
            .filter(|limit| *limit > 0)
    }
}

/// [`ModelLimits::SEEDED`] as a lookup table, shared by every profile until it learns a limit.
static SEEDED_LIMITS: LazyLock<Arc<ProfileLimits>> = LazyLock::new(|| {
    Arc::new(
        ModelLimits::SEEDED
            .iter()
            .map(|(model, limit)| {
                let entry = ModelLimit {
                    limit: *limit,
                    source: LimitSource::Seeded,
                    learned_at: 0,
                };
                ((*model).to_string(), entry)
            })
            .collect(),
    )
});

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = "copilot";

    fn rejection(code: &str, message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"error": {"code": code, "message": message}}))
            .unwrap()
    }

    #[test]
    fn limit_is_read_from_a_prompt_too_long_rejection() {
        let body = rejection(
            "model_max_prompt_tokens_exceeded",
            "prompt is too long: 1000123 tokens > 1000000 maximum",
        );
        assert_eq!(ModelLimits::limit_from_error(&body), Some(1_000_000));
    }

    #[test]
    fn limit_with_thousands_separators_is_read() {
        let body = rejection(
            "model_max_prompt_tokens_exceeded",
            "prompt is too long: 250,001 tokens > 200,000 maximum",
        );
        assert_eq!(ModelLimits::limit_from_error(&body), Some(200_000));
    }

    #[test]
    fn other_errors_teach_nothing() {
        let other_code = rejection("rate_limited", "1 tokens > 5 maximum");
        let other_message = rejection("model_max_prompt_tokens_exceeded", "prompt is too long");
        let no_marker = rejection("model_max_prompt_tokens_exceeded", "a > 5 tokens");
        assert_eq!(ModelLimits::limit_from_error(&other_code), None);
        assert_eq!(ModelLimits::limit_from_error(&other_message), None);
        assert_eq!(ModelLimits::limit_from_error(&no_marker), None);
        assert_eq!(ModelLimits::limit_from_error(b"not json"), None);
    }

    #[test]
    fn unknown_profile_gets_the_seeded_limits() {
        let limits = ModelLimits::default();
        let profile = limits.for_profile(PROFILE);
        assert_eq!(profile["claude-opus-5.5"].limit, 1_000_000);
        assert_eq!(profile["claude-haiku-4.5"].limit, 200_000);
        assert_eq!(profile["claude-haiku-4.5"].source, LimitSource::Seeded);
        assert!(!profile.contains_key("gpt-6.1-sol"));
    }

    #[test]
    fn learning_overwrites_only_a_different_limit() {
        let limits = ModelLimits::default();
        assert_eq!(limits.learn(PROFILE, "claude-opus-5.5", 1_000_000, 7), None);
        let (entry, previous) = limits
            .learn(PROFILE, "claude-opus-5.5", 900_000, 7)
            .unwrap();
        assert_eq!(entry.source, LimitSource::Learned);
        assert_eq!(previous, Some(1_000_000));
        assert_eq!(
            limits.for_profile(PROFILE)["claude-opus-5.5"].limit,
            900_000
        );
        assert_eq!(
            limits.for_profile(PROFILE)["claude-haiku-4.5"].limit,
            200_000
        );
        assert_eq!(
            limits.for_profile("other")["claude-opus-5.5"].limit,
            1_000_000
        );
        assert_eq!(limits.learn(PROFILE, "claude-opus-5.5", 900_000, 8), None);
    }

    #[test]
    fn learned_limits_survive_a_restart_and_seeds_never_replace_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let path = path.to_str().unwrap();
        {
            let store = Store::open("sqlite", Some(path)).unwrap();
            let limits = ModelLimits::load(Some(&store), [PROFILE], 1);
            let (entry, _) = limits.learn(PROFILE, "gpt-6.1-sol", 2_000_000, 2).unwrap();
            store
                .save_model_limit(PROFILE, "gpt-6.1-sol", &entry)
                .unwrap();
            let (entry, _) = limits
                .learn(PROFILE, "claude-haiku-4.5", 150_000, 3)
                .unwrap();
            store
                .save_model_limit(PROFILE, "claude-haiku-4.5", &entry)
                .unwrap();
        }
        let store = Store::open("sqlite", Some(path)).unwrap();
        let limits = ModelLimits::load(Some(&store), [PROFILE], 4).for_profile(PROFILE);
        assert_eq!(limits["gpt-6.1-sol"].limit, 2_000_000);
        assert_eq!(limits["gpt-6.1-sol"].learned_at, 2);
        assert_eq!(limits["claude-haiku-4.5"].limit, 150_000);
        assert_eq!(limits["claude-haiku-4.5"].source, LimitSource::Learned);
        assert_eq!(limits["claude-opus-5.5"].source, LimitSource::Seeded);
    }
}

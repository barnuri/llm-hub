//! Local model tuner: measures prefill speed of local models, proposes llama-swap flag changes
//! and applies them to the model registry only on approval.

pub mod approve;
pub mod command;
pub mod llama_swap;
pub mod native;
pub mod prefill;
pub mod probe;
pub mod registry;
pub mod rules;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use dashmap::DashMap;

use self::prefill::PrefillFit;
use crate::services::store::now_ms;

/// How long a served prompt counts as still cached in the model's single slot.
const WARM_TTL_MS: u64 = 30 * 60 * 1_000;

/// The last prompt a local model served, which llama-server keeps in its KV cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WarmPrefix {
    /// Prompt tokens of that request.
    tokens: u64,
    /// When the request finished, in Unix milliseconds.
    at_ms: u64,
}

/// The tuner's in-memory state: prefill fits per model and one apply lock per profile.
#[derive(Default)]
pub struct LocalTuner {
    /// Qualified model (`<profile>/<model>`) -> its fitted prefill curve, swapped whole by the probe.
    fits: ArcSwap<HashMap<String, PrefillFit>>,
    /// Profile -> the lock an approval holds while it edits that profile's registry.
    apply_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Qualified model -> the prompt it last served, so a follow-up turn counts only its new suffix.
    warm: DashMap<String, WarmPrefix>,
}

impl fmt::Debug for LocalTuner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalTuner")
            .field("fits", &self.fits.load().len())
            .field("warm", &self.warm.len())
            .finish_non_exhaustive()
    }
}

impl LocalTuner {
    /// Replaces every fit with `fits`, keyed by qualified model.
    pub fn set_fits(&self, fits: HashMap<String, PrefillFit>) {
        self.fits.store(Arc::new(fits));
    }

    /// The expected uncached prefill time for `qualified` and `prompt_tokens`, when the fit is confident.
    #[must_use]
    pub fn prefill_budget(&self, qualified: &str, prompt_tokens: u64) -> Option<u64> {
        let fit = self.fits.load().get(qualified).copied()?;
        fit.confident_at(prompt_tokens)
            .then(|| fit.estimate_ms(prompt_tokens))
    }

    /// The fit of `qualified`, when the probe measured one.
    #[cfg(test)]
    #[must_use]
    pub fn fit(&self, qualified: &str) -> Option<PrefillFit> {
        self.fits.load().get(qualified).copied()
    }

    /// Records that `qualified` served a prompt of `prompt_tokens`, which stays warm in its KV cache.
    /// The caller passes the hub's byte estimate, the unit the guard compares. One map insert.
    pub fn observe(&self, qualified: &str, prompt_tokens: u64, now_ms: u64) {
        if prompt_tokens > 0 {
            self.warm.insert(
                qualified.to_owned(),
                WarmPrefix {
                    tokens: prompt_tokens,
                    at_ms: now_ms,
                },
            );
        }
    }

    /// Why `qualified` cannot prefill `prompt_tokens` within `budget_ms`, or `None` when it can, or
    /// when no confident fit covers that size. Reads memory only, so the request path may call it.
    #[must_use]
    pub fn prefill_refusal(
        &self,
        qualified: &str,
        prompt_tokens: u64,
        budget_ms: u64,
    ) -> Option<String> {
        self.prefill_refusal_at(qualified, prompt_tokens, budget_ms, now_ms())
    }

    /// [`Self::prefill_refusal`] at `now_ms`. A prompt that extends the model's warm prefix pays
    /// only for its new tokens: the fit's cost at the full depth minus its cost at the prefix depth.
    fn prefill_refusal_at(
        &self,
        qualified: &str,
        prompt_tokens: u64,
        budget_ms: u64,
        now_ms: u64,
    ) -> Option<String> {
        let fits = self.fits.load();
        let fit = fits.get(qualified)?;
        if !fit.confident_at(prompt_tokens) {
            return None;
        }
        let warm_tokens = self.warm_tokens(qualified, prompt_tokens, now_ms);
        let expected_ms = fit
            .estimate_ms(prompt_tokens)
            .saturating_sub(warm_tokens.map_or(0, |tokens| fit.estimate_ms(tokens)));
        if expected_ms <= budget_ms {
            return None;
        }
        let rate = fit.marginal_tok_per_s(prompt_tokens).round();
        let new_tokens = warm_tokens.map_or(String::new(), |tokens| {
            format!(
                " (~{} new tokens after a warm prefix)",
                tokens_label(prompt_tokens - tokens)
            )
        });
        Some(format!(
            "{qualified}: prompt ~{} tokens{new_tokens}, expected ~{} to prefill at {rate} tok/s \
             (measured); budget {}. Use a smaller prompt or another model",
            tokens_label(prompt_tokens),
            duration_label(expected_ms),
            duration_label(budget_ms),
        ))
    }

    /// The size of the warm prefix a prompt of `prompt_tokens` can reuse at `now_ms`: the model's
    /// last served prompt, when it is within [`WARM_TTL_MS`] and no larger than this one.
    fn warm_tokens(&self, qualified: &str, prompt_tokens: u64, now_ms: u64) -> Option<u64> {
        let warm = *self.warm.get(qualified)?;
        (now_ms.saturating_sub(warm.at_ms) <= WARM_TTL_MS && prompt_tokens >= warm.tokens)
            .then_some(warm.tokens)
    }

    /// The lock an approval for `profile` takes, so two applies never edit one registry at once.
    #[must_use]
    pub fn apply_lock(&self, profile: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .apply_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(profile.to_owned()).or_default())
    }
}

/// `94k` from 94 000 tokens, or the plain count under a thousand.
fn tokens_label(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        tokens.to_string()
    }
}

/// `21 min` from minutes and up, rounded; `45 s` below a minute, rounded up.
fn duration_label(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{} min", (ms + 30_000) / 60_000)
    } else {
        format!("{} s", ms.div_ceil(1_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// About 286 tok/s on short prompts, slowing to about 73 tok/s near 94k tokens.
    fn slow_fit(low_confidence: bool) -> PrefillFit {
        PrefillFit {
            a: 3.5,
            b: 5.5e-5,
            low_confidence,
            max_tokens_seen: 100_000,
        }
    }

    fn tuner(fit: PrefillFit) -> LocalTuner {
        let tuner = LocalTuner::default();
        tuner.set_fits(HashMap::from([("local/gemma".to_owned(), fit)]));
        tuner
    }

    #[test]
    fn refuses_a_prompt_over_budget_with_the_numbers() {
        let tuner = tuner(slow_fit(false));
        let message = tuner
            .prefill_refusal("local/gemma", 94_000, 300_000)
            .unwrap();
        assert_eq!(
            message,
            "local/gemma: prompt ~94k tokens, expected ~14 min to prefill at 72 tok/s \
             (measured); budget 5 min. Use a smaller prompt or another model"
        );
    }

    #[test]
    fn a_warm_prefix_within_the_ttl_counts_only_the_new_tokens() {
        let tuner = tuner(slow_fit(false));
        tuner.observe("local/gemma", 90_000, 1_000);
        // 4k new tokens on a 90k prefix cost about 55 s; the full 94k would take about 14 min.
        assert_eq!(
            tuner.prefill_refusal_at("local/gemma", 94_000, 60_000, 1_000 + WARM_TTL_MS),
            None
        );
        let message = tuner
            .prefill_refusal_at("local/gemma", 94_000, 30_000, 1_000)
            .unwrap();
        assert_eq!(
            message,
            "local/gemma: prompt ~94k tokens (~4k new tokens after a warm prefix), expected ~55 s \
             to prefill at 72 tok/s (measured); budget 30 s. Use a smaller prompt or another model"
        );
    }

    #[test]
    fn an_expired_or_larger_warm_prefix_counts_the_full_prompt() {
        let tuner = tuner(slow_fit(false));
        tuner.observe("local/gemma", 90_000, 1_000);
        let expired = tuner
            .prefill_refusal_at("local/gemma", 94_000, 60_000, 1_001 + WARM_TTL_MS)
            .unwrap();
        assert!(expired.contains("expected ~14 min"), "{expired}");
        assert!(!expired.contains("warm prefix"), "{expired}");
        // A prompt smaller than the warm one starts a new conversation.
        let smaller = tuner
            .prefill_refusal_at("local/gemma", 80_000, 60_000, 1_000)
            .unwrap();
        assert!(!smaller.contains("warm prefix"), "{smaller}");
    }

    #[test]
    fn an_empty_prompt_is_not_recorded() {
        let tuner = tuner(slow_fit(false));
        tuner.observe("local/gemma", 0, 1_000);
        assert!(
            tuner
                .prefill_refusal_at("local/gemma", 94_000, 60_000, 1_000)
                .is_some()
        );
        assert!(tuner.warm.is_empty());
    }

    #[test]
    fn allows_a_prompt_under_budget() {
        let tuner = tuner(slow_fit(false));
        assert_eq!(tuner.prefill_refusal("local/gemma", 8_000, 300_000), None);
    }

    #[test]
    fn no_fit_never_refuses() {
        let tuner = tuner(slow_fit(false));
        assert_eq!(tuner.prefill_refusal("local/qwen", 94_000, 1), None);
        assert_eq!(tuner.fit("local/qwen"), None);
        assert!(tuner.fit("local/gemma").is_some());
    }

    #[test]
    fn low_confidence_or_far_extrapolation_never_refuses() {
        assert_eq!(
            tuner(slow_fit(true)).prefill_refusal("local/gemma", 94_000, 1),
            None
        );
        assert_eq!(
            tuner(slow_fit(false)).prefill_refusal("local/gemma", 900_000, 1),
            None
        );
    }

    #[test]
    fn labels_round_for_people() {
        assert_eq!(tokens_label(940), "940");
        assert_eq!(tokens_label(1_499), "1k");
        assert_eq!(duration_label(44_100), "45 s");
        assert_eq!(duration_label(60_000), "1 min");
        assert_eq!(duration_label(1_290_000), "22 min");
    }

    #[tokio::test]
    async fn apply_lock_is_shared_per_profile() {
        let tuner = LocalTuner::default();
        let held = tuner.apply_lock("local").try_lock_owned().unwrap();
        assert!(tuner.apply_lock("local").try_lock_owned().is_err());
        assert!(tuner.apply_lock("other").try_lock_owned().is_ok());
        drop(held);
        assert!(tuner.apply_lock("local").try_lock_owned().is_ok());
        assert!(format!("{tuner:?}").contains("LocalTuner"));
    }
}

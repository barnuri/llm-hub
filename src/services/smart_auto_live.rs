use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::dependencies::state::AppState;
use crate::schemas::live_signals::{LiveSignals, ModelSignal};
use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::services::pricing::PricingBook;
use crate::services::store::now_ms;

/// The live signals of one request's pool, and the models to leave out of it.
#[derive(Debug, Default)]
pub struct CollectedSignals {
    pub signals: LiveSignals,
    /// Cooled after a failure.
    pub unavailable: HashSet<String>,
}

/// Turns what the hub measured (failures, speed, price book, quota) into router inputs.
/// Everything is read from memory: no network call, no disk read.
#[derive(Debug)]
pub struct SmartAutoLive;

impl SmartAutoLive {
    /// A quota snapshot older than this is not trusted.
    const QUOTA_TTL_MS: u64 = 3_600_000;
    const TOKENS_PER_MILLION_INPUT_SHARE: f64 = 3.0;
    const IO_SHARES: f64 = 4.0;
    /// Upper bound of each cost score, from 10 (cheapest) down to 1, in USD per million tokens.
    const COST_BOUNDS_USD: [f64; 10] = [0.10, 0.50, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0, 100.0];
    const COST_FREE: u8 = 10;

    /// The signals of every model in `pool` on `endpoint`.
    #[must_use]
    pub fn collect(
        state: &AppState,
        endpoint: &str,
        pool: &[&ModelDescriptor],
        now: Instant,
    ) -> CollectedSignals {
        let health = state.model_health().view(endpoint, now);
        let costs = state.model_costs();
        let mut quotas: HashMap<&str, Option<f64>> = HashMap::new();
        let mut by_model = HashMap::with_capacity(pool.len());
        let unavailable = health.cooled;
        for descriptor in pool {
            let id = strip_1m_suffix(&descriptor.qualified_id);
            let quota = *quotas
                .entry(descriptor.profile.as_str())
                .or_insert_with(|| Self::quota_left(state, &descriptor.profile));
            by_model.insert(
                id.to_owned(),
                ModelSignal {
                    cost: costs.get(id).copied(),
                    speed: health.speeds.get(id).copied(),
                    error_rate: health.error_rates.get(id).copied(),
                    quota_remaining_pct: quota,
                },
            );
        }
        CollectedSignals {
            signals: LiveSignals::new(by_model),
            unavailable,
        }
    }

    /// Cost scores of every model, from the price book. A model with no known price has none.
    #[must_use]
    pub fn cost_scores<'a>(
        book: &PricingBook,
        model_ids: impl Iterator<Item = &'a str>,
    ) -> HashMap<String, u8> {
        let mut scores = HashMap::new();
        for raw in model_ids {
            let id = strip_1m_suffix(raw);
            let rates = book.rates_for(id);
            let score = if rates.is_zero() {
                let profile = id.split_once('/').map_or("", |(profile, _)| profile);
                book.local_profiles
                    .contains(profile)
                    .then_some(Self::COST_FREE)
            } else {
                let blended = (Self::TOKENS_PER_MILLION_INPUT_SHARE * rates.input_per_1m
                    + rates.output_per_1m)
                    / Self::IO_SHARES;
                Some(Self::cost_score(blended))
            };
            if let Some(score) = score {
                scores.insert(id.to_owned(), score);
            }
        }
        scores
    }

    /// The cost score of a blended price in USD per million tokens (three parts input, one output).
    #[must_use]
    pub fn cost_score(blended_usd_per_1m: f64) -> u8 {
        Self::COST_BOUNDS_USD
            .iter()
            .position(|bound| blended_usd_per_1m <= *bound)
            .map_or(0, |index| {
                Self::COST_FREE.saturating_sub(u8::try_from(index).unwrap_or(u8::MAX))
            })
    }

    /// The least quota left on `profile`, in percent, or `None` without a fresh snapshot. A quota
    /// is a score penalty only: a model it does not meter would otherwise be dropped with the
    /// rest, and a metered model that is refused is cooled by its own error.
    fn quota_left(state: &AppState, profile: &str) -> Option<f64> {
        let usage = state.profile_usage().cached(profile)?;
        if now_ms().saturating_sub(usage.updated_at) > Self::QUOTA_TTL_MS {
            return None;
        }
        usage
            .quotas
            .iter()
            .map(|quota| quota.remaining_percentage)
            .reduce(f64::min)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::token_rates::TokenRates;

    fn book() -> PricingBook {
        PricingBook {
            by_model: HashMap::from([
                (
                    "acme/cheap".to_owned(),
                    TokenRates {
                        input_per_1m: 0.15,
                        output_per_1m: 0.6,
                        cache_read_per_1m: 0.0,
                        cache_write_per_1m: 0.0,
                    },
                ),
                (
                    "acme/dear".to_owned(),
                    TokenRates {
                        input_per_1m: 15.0,
                        output_per_1m: 75.0,
                        cache_read_per_1m: 0.0,
                        cache_write_per_1m: 0.0,
                    },
                ),
            ]),
            local_profiles: HashSet::from(["home".to_owned()]),
            ..PricingBook::default()
        }
    }

    #[test]
    fn a_cheaper_model_scores_higher() {
        let scores =
            SmartAutoLive::cost_scores(&book(), ["acme/cheap", "acme/dear[1m]"].into_iter());
        assert_eq!(scores["acme/cheap"], 9);
        assert_eq!(scores["acme/dear"], 3);
    }

    #[test]
    fn a_local_model_is_free_and_an_unpriced_remote_one_is_unknown() {
        let scores =
            SmartAutoLive::cost_scores(&book(), ["home/qwen", "other/unknown"].into_iter());
        assert_eq!(scores["home/qwen"], 10);
        assert!(!scores.contains_key("other/unknown"));
    }

    #[test]
    fn the_cost_scale_runs_from_ten_to_zero() {
        let scores: Vec<u8> = [0.05, 0.3, 0.9, 1.5, 3.0, 6.0, 12.0, 20.0, 50.0, 90.0, 500.0]
            .into_iter()
            .map(SmartAutoLive::cost_score)
            .collect();
        assert_eq!(scores, vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
    }
}

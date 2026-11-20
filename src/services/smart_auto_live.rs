use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::configs::TokenRates;
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
    /// An agent turn re-reads its cached prompt, so the blend weighs cached input most: eight
    /// parts cached input, one part fresh input and one part output.
    const CACHED_INPUT_SHARE: f64 = 8.0;
    const FRESH_INPUT_SHARE: f64 = 1.0;
    const OUTPUT_SHARE: f64 = 1.0;
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
        let mut unavailable = health.cooled;
        for descriptor in pool {
            let id = strip_1m_suffix(&descriptor.qualified_id);
            if health.cooled_profiles.contains(&descriptor.profile) {
                unavailable.insert(id.to_owned());
            }
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
                Some(Self::cost_score(Self::blended_usd_per_1m(&rates)))
            };
            if let Some(score) = score {
                scores.insert(id.to_owned(), score);
            }
        }
        scores
    }

    /// USD per million tokens of an agent turn. A model with no cache-read price reads cache at
    /// its input price.
    fn blended_usd_per_1m(rates: &TokenRates) -> f64 {
        let cached = if rates.cache_read_per_1m > 0.0 {
            rates.cache_read_per_1m
        } else {
            rates.input_per_1m
        };
        (Self::CACHED_INPUT_SHARE * cached
            + Self::FRESH_INPUT_SHARE * rates.input_per_1m
            + Self::OUTPUT_SHARE * rates.output_per_1m)
            / (Self::CACHED_INPUT_SHARE + Self::FRESH_INPUT_SHARE + Self::OUTPUT_SHARE)
    }

    /// The cost score of a blended price in USD per million tokens (see `blended_usd_per_1m`).
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
    use std::sync::Arc;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::configs::token_rates::TokenRates;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::model_health::FailureKind;
    use crate::services::profile_usage::{self, UsageKey};

    const ENDPOINT: &str = "/v1/chat/completions";
    const QUOTA_PROFILE: &str = "llmgw";
    const HOUR_MS: u64 = 3_600_000;

    fn state(quota_base_url: &str) -> AppState {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "home,remote,llmgw"),
            ("LLM_HUB_HOME_BASE_URL", "http://127.0.0.1:9"),
            (
                "LLM_HUB_REMOTE_BASE_URL",
                "https://remote.example.invalid/v1",
            ),
            ("LLM_HUB_LLMGW_BASE_URL", quota_base_url),
            ("LLM_HUB_LLMGW_API_KEY", "sk-bf-secret"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    async fn quota_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/governance/virtual-keys/quota"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "budgets": [
                    {"max_limit": 100.0, "current_usage": 40.0, "reset_duration": "1M"},
                    {"max_limit": 10.0, "current_usage": 8.0, "reset_duration": "1d"},
                ],
            })))
            .mount(&server)
            .await;
        server
    }

    async fn seed_quota(state: &AppState, fetched_at_ms: u64) {
        let key = UsageKey::for_profile(&state.config(), QUOTA_PROFILE).unwrap();
        let client = state.client(QUOTA_PROFILE).unwrap();
        profile_usage::usage_at(state, &client, QUOTA_PROFILE, &key, fetched_at_ms)
            .await
            .expect("quota fetched");
    }

    fn descriptors(ids: &[&str]) -> Vec<ModelDescriptor> {
        ids.iter().map(|id| ModelDescriptor::for_test(id)).collect()
    }

    #[tokio::test]
    async fn collect_reads_health_cost_and_the_least_quota_left() {
        let server = quota_server().await;
        let state = state(&format!("{}/v1", server.uri()));
        let pool = descriptors(&["home/cooled", "home/fast[1m]", "remote/m", "llmgw/m"]);
        state.set_model_descriptors(pool.clone());
        seed_quota(&state, now_ms()).await;
        let now = Instant::now();
        let health = state.model_health();
        health.record_failure("home/cooled", ENDPOINT, FailureKind::Status(503), now);
        for _ in 0..5 {
            health.record_ttft("home/fast", ENDPOINT, 500, now);
            health.record_success("home/fast", ENDPOINT, now);
        }
        let pool_refs: Vec<&ModelDescriptor> = pool.iter().collect();

        let collected = SmartAutoLive::collect(&state, ENDPOINT, &pool_refs, now);

        assert_eq!(
            collected.unavailable,
            HashSet::from(["home/cooled".to_owned()])
        );
        let fast = collected.signals.get("home/fast[1m]");
        assert_eq!(fast.cost, Some(10));
        assert_eq!(fast.speed, Some(10));
        assert_eq!(fast.error_rate, Some(0.0));
        assert_eq!(fast.quota_remaining_pct, None);
        let remote = collected.signals.get("remote/m");
        assert_eq!((remote.cost, remote.speed), (None, None));
        assert_eq!(
            collected.signals.get("llmgw/m").quota_remaining_pct,
            Some(20.0)
        );
    }

    #[tokio::test]
    async fn a_cooled_profile_makes_every_model_of_it_unavailable() {
        let state = state("http://127.0.0.1:9/v1");
        let pool = descriptors(&["home/a", "home/b[1m]", "remote/m"]);
        let now = Instant::now();
        state.model_health().cool_profile(
            "home",
            std::time::Duration::from_hours(1),
            "out of credits".into(),
            now,
        );
        let pool_refs: Vec<&ModelDescriptor> = pool.iter().collect();

        let collected = SmartAutoLive::collect(&state, ENDPOINT, &pool_refs, now);

        assert_eq!(
            collected.unavailable,
            HashSet::from(["home/a".to_owned(), "home/b".to_owned()])
        );
    }

    #[tokio::test]
    async fn health_on_another_endpoint_does_not_cool_a_model() {
        let state = state("http://127.0.0.1:9/v1");
        let pool = descriptors(&["home/m"]);
        let now = Instant::now();
        state
            .model_health()
            .record_failure("home/m", "/v1/messages", FailureKind::Transport, now);
        let pool_refs: Vec<&ModelDescriptor> = pool.iter().collect();

        let collected = SmartAutoLive::collect(&state, ENDPOINT, &pool_refs, now);

        assert!(collected.unavailable.is_empty());
        assert_eq!(collected.signals.get("home/m").error_rate, None);
    }

    #[tokio::test]
    async fn a_stale_quota_snapshot_is_not_trusted() {
        let server = quota_server().await;
        let state = state(&format!("{}/v1", server.uri()));
        seed_quota(&state, now_ms() - 2 * HOUR_MS).await;

        let quota = SmartAutoLive::quota_left(&state, QUOTA_PROFILE);

        assert_eq!(quota, None);
    }

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
    fn a_cheap_cache_read_price_raises_the_cost_score() {
        let rates = |cache_read_per_1m| TokenRates {
            input_per_1m: 5.0,
            output_per_1m: 25.0,
            cache_read_per_1m,
            cache_write_per_1m: 0.0,
        };
        let mut priced = book();
        priced.by_model.insert("acme/cached".to_owned(), rates(0.5));
        priced
            .by_model
            .insert("acme/uncached".to_owned(), rates(0.0));

        let scores =
            SmartAutoLive::cost_scores(&priced, ["acme/cached", "acme/uncached"].into_iter());

        // (8 x 0.5 + 5 + 25) / 10 = 3.4 against (8 x 5 + 5 + 25) / 10 = 7.0.
        assert_eq!(scores["acme/cached"], 6);
        assert_eq!(scores["acme/uncached"], 5);
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

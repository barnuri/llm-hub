//! Estimated USD cost from token counts.
//!
//! Rates are USD per 1M tokens. Lookup order for a qualified `profile/model`:
//! 1. Exact model override from env
//! 2. Bare model id override
//! 3. Profile default rates
//! 4. $0 for a local profile
//! 5. Copilot's published price, for a Copilot profile
//! 6. Built-in cloud model guesses (when nothing else is set)
//! 7. Global hub defaults
//! 8. $0 (unknown)

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::configs::{HubConfig, TokenRates};
use crate::schemas::usage_report::UsageReport;
use crate::services::copilot_auto::AutoTier;
use crate::services::stats::StatsSnapshot;

const PER_MILLION: f64 = 1_000_000.0;
const LONG_CONTEXT_SUFFIX: &str = "[1m]";
const FREE_TAG: &str = "free";
/// Per-model price object on each `/v1/models` entry.
const PRICE_FIELD: &str = "llm_hub_price";

#[derive(Debug, Clone, Default)]
pub struct PricingBook {
    pub global: TokenRates,
    pub by_profile: HashMap<String, TokenRates>,
    /// Keys may be qualified (`profile/model`) or bare model ids.
    pub by_model: HashMap<String, TokenRates>,
    pub local_profiles: HashSet<String>,
    pub copilot_profiles: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PricingMeta {
    pub configured: bool,
    pub note: String,
}

impl PricingBook {
    #[must_use]
    pub fn from_hub(config: &HubConfig) -> Self {
        let mut book = Self {
            global: config.pricing.with_cache_defaults(),
            by_profile: HashMap::new(),
            by_model: HashMap::new(),
            local_profiles: HashSet::new(),
            copilot_profiles: HashSet::new(),
        };
        for profile in &config.profiles {
            if profile.is_local() {
                book.local_profiles.insert(profile.name.clone());
            }
            if profile.copilot.is_some() {
                book.copilot_profiles.insert(profile.name.clone());
            }
            let rates = profile.pricing.with_cache_defaults();
            if !rates.is_zero() {
                book.by_profile.insert(profile.name.clone(), rates);
            }
            for (model, model_rates) in &profile.model_prices {
                let rates = model_rates.with_cache_defaults();
                book.by_model
                    .insert(format!("{}/{}", profile.name, model), rates);
                book.by_model.insert(model.clone(), rates);
            }
        }
        book
    }

    #[must_use]
    pub fn rates_for(&self, model_key: &str) -> TokenRates {
        if let Some(rates) = self.by_model.get(model_key) {
            return *rates;
        }
        let (profile, bare) = split_model_key(model_key);
        if let Some(bare) = bare
            && let Some(rates) = self.by_model.get(bare)
        {
            return *rates;
        }
        if let Some(profile) = profile
            && let Some(rates) = self.by_profile.get(profile)
            && !rates.is_zero()
        {
            return *rates;
        }
        if profile.is_some_and(|profile| self.local_profiles.contains(profile)) {
            return TokenRates::default();
        }
        if let Some(profile) = profile
            && self.copilot_profiles.contains(profile)
            && let Some(rates) = bare.and_then(copilot_rates)
        {
            return rates.with_cache_defaults();
        }
        if let Some(bare) = bare.or(Some(model_key))
            && let Some(rates) = builtin_rates(bare)
        {
            return rates.with_cache_defaults();
        }
        self.global.with_cache_defaults()
    }

    #[must_use]
    pub fn estimate(
        &self,
        model_key: &str,
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        estimate_with_rates(
            self.rates_for(model_key),
            tokens_in,
            tokens_out,
            cache_read,
            cache_write,
        )
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        if !self.global.is_zero() {
            return true;
        }
        self.by_profile.values().any(|r| !r.is_zero())
            || self.by_model.values().any(|r| !r.is_zero())
    }
}

/// Attach `cost_usd` to every row. Profile + overview costs sum model rows so
/// mixed model prices stay accurate.
pub fn apply_costs(snapshot: &mut StatsSnapshot, book: &PricingBook) {
    for row in &mut snapshot.models {
        row.cost_usd = book.estimate(
            &row.key,
            row.tokens_in,
            row.tokens_out,
            row.cache_read_tokens,
            row.cache_write_tokens,
        );
    }
    for row in &mut snapshot.profiles {
        let prefix = format!("{}/", row.key);
        row.cost_usd = snapshot
            .models
            .iter()
            .filter(|model| model.key == row.key || model.key.starts_with(&prefix))
            .map(|model| model.cost_usd)
            .sum();
        // Profile with traffic but no model rows (cardinality overflow edge): fall back.
        if row.cost_usd == 0.0 && (row.tokens_in > 0 || row.tokens_out > 0) {
            row.cost_usd = book.estimate(
                &row.key,
                row.tokens_in,
                row.tokens_out,
                row.cache_read_tokens,
                row.cache_write_tokens,
            );
        }
    }
    snapshot.overview.cost_usd = snapshot.models.iter().map(|row| row.cost_usd).sum();
    for point in &mut snapshot.series {
        point.cost_usd = book.estimate(
            &point.key,
            point.tokens_in,
            point.tokens_out,
            point.cache_read_tokens,
            point.cache_write_tokens,
        );
    }
    snapshot.pricing = PricingMeta {
        configured: book.is_configured() || snapshot.overview.cost_usd > 0.0,
        note: if book.is_configured() {
            "Estimated from configured USD/1M token rates (and built-in cloud guesses when unset)."
                .into()
        } else if snapshot.overview.cost_usd > 0.0 {
            "Estimated from built-in cloud model price guesses. Set LLM_HUB_*_INPUT_USD_PER_1M to override."
                .into()
        } else {
            "No pricing configured — costs stay $0 until you set LLM_HUB_<PROFILE>_INPUT_USD_PER_1M / OUTPUT_USD_PER_1M (or global LLM_HUB_INPUT_USD_PER_1M)."
                .into()
        },
    };
}

/// Appends each model's price to its `display_name` (`· $in/$out` per 1M tokens, or `· free`
/// for a local profile) and adds `llm_hub_price`. Auto selectors and unpriced models stay as they are.
pub fn annotate_listing(models: &mut [Value], book: &PricingBook) {
    for model in models {
        let Some(obj) = model.as_object_mut() else {
            continue;
        };
        let Some(id) = obj.get("id").and_then(Value::as_str) else {
            continue;
        };
        let key = id.strip_suffix(LONG_CONTEXT_SUFFIX).unwrap_or(id);
        let Some((profile, bare)) = key.split_once('/') else {
            continue;
        };
        if AutoTier::from_model(bare).is_some() {
            continue;
        }
        let rates = book.rates_for(key);
        let tag = if !rates.is_zero() {
            format!("{}/{}", usd(rates.input_per_1m), usd(rates.output_per_1m))
        } else if book.local_profiles.contains(profile) {
            FREE_TAG.to_string()
        } else {
            continue;
        };
        if let Some(name) = obj.get("display_name").and_then(Value::as_str) {
            let labelled = format!("{name} · {tag}");
            obj.insert("display_name".into(), json!(labelled));
        }
        obj.insert(
            PRICE_FIELD.into(),
            json!({
                "input_usd_per_1m": rates.input_per_1m,
                "output_usd_per_1m": rates.output_per_1m,
                "cache_read_usd_per_1m": rates.cache_read_per_1m,
                "cache_write_usd_per_1m": rates.cache_write_per_1m,
            }),
        );
    }
}

/// `4.0` → `$4`, `0.125` → `$0.125`.
fn usd(amount: f64) -> String {
    let fixed = format!("{amount:.3}");
    format!("${}", fixed.trim_end_matches('0').trim_end_matches('.'))
}

/// Attach estimated `cost_usd` to each usage log row. Local / unknown models stay `$0`.
pub fn apply_usage_costs(report: &mut UsageReport, book: &PricingBook) {
    for row in &mut report.recent {
        row.cost_usd = book.estimate(
            &row.model,
            row.tokens_in,
            row.tokens_out,
            row.cache_read_tokens,
            row.cache_write_tokens,
        );
    }
}

#[allow(clippy::cast_precision_loss)] // token counts → USD; mantissa loss is fine for estimates
pub fn estimate_with_rates(
    rates: TokenRates,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    let rates = rates.with_cache_defaults();
    let (uncached_in, cached_in) = split_cached_input(tokens_in, cache_read);
    let input = uncached_in as f64 * rates.input_per_1m / PER_MILLION;
    let cached = cached_in as f64 * rates.cache_read_per_1m / PER_MILLION;
    let written = cache_write as f64 * rates.cache_write_per_1m / PER_MILLION;
    let output = tokens_out as f64 * rates.output_per_1m / PER_MILLION;
    input + cached + written + output
}

/// Splits reported input into `(uncached, cached)` prompt tokens. A cache read larger
/// than the input was reported apart from it (Anthropic), not as a subset (`OpenAI`).
#[must_use]
pub fn split_cached_input(tokens_in: u64, cache_read: u64) -> (u64, u64) {
    if cache_read > tokens_in {
        (tokens_in, cache_read)
    } else {
        (tokens_in - cache_read, cache_read)
    }
}

fn split_model_key(model_key: &str) -> (Option<&str>, Option<&str>) {
    match model_key.split_once('/') {
        Some((profile, rest)) if !profile.is_empty() && !rest.is_empty() => {
            (Some(profile), Some(rest))
        }
        _ => (None, None),
    }
}

fn builtin_rates(bare_model: &str) -> Option<TokenRates> {
    let id = bare_model.to_ascii_lowercase();
    let table: &[(&str, TokenRates)] = &[
        (
            "gpt-4o-mini",
            TokenRates {
                input_per_1m: 0.15,
                output_per_1m: 0.60,
                cache_read_per_1m: 0.075,
                cache_write_per_1m: 0.15,
            },
        ),
        (
            "gpt-4o",
            TokenRates {
                input_per_1m: 2.50,
                output_per_1m: 10.0,
                cache_read_per_1m: 1.25,
                cache_write_per_1m: 2.50,
            },
        ),
        (
            "gpt-4.1-mini",
            TokenRates {
                input_per_1m: 0.40,
                output_per_1m: 1.60,
                cache_read_per_1m: 0.10,
                cache_write_per_1m: 0.40,
            },
        ),
        (
            "gpt-4.1",
            TokenRates {
                input_per_1m: 2.0,
                output_per_1m: 8.0,
                cache_read_per_1m: 0.50,
                cache_write_per_1m: 2.0,
            },
        ),
        (
            "o3-mini",
            TokenRates {
                input_per_1m: 1.10,
                output_per_1m: 4.40,
                cache_read_per_1m: 0.55,
                cache_write_per_1m: 1.10,
            },
        ),
        (
            "claude-sonnet",
            TokenRates {
                input_per_1m: 3.0,
                output_per_1m: 15.0,
                cache_read_per_1m: 0.30,
                cache_write_per_1m: 3.75,
            },
        ),
        (
            "claude-haiku",
            TokenRates {
                input_per_1m: 0.80,
                output_per_1m: 4.0,
                cache_read_per_1m: 0.08,
                cache_write_per_1m: 1.0,
            },
        ),
        (
            "claude-opus",
            TokenRates {
                input_per_1m: 15.0,
                output_per_1m: 75.0,
                cache_read_per_1m: 1.50,
                cache_write_per_1m: 18.75,
            },
        ),
    ];
    table
        .iter()
        .find(|(needle, _)| id.contains(needle))
        .map(|(_, rates)| *rates)
}

const fn rates(input: f64, cache_read: f64, cache_write: f64, output: f64) -> TokenRates {
    TokenRates {
        input_per_1m: input,
        output_per_1m: output,
        cache_read_per_1m: cache_read,
        cache_write_per_1m: cache_write,
    }
}

/// GitHub Copilot's default-tier USD per 1M tokens (1 AI credit = $0.01), keyed by the
/// Copilot id with dots as dashes. A zero cache write falls back to the input rate.
const COPILOT_PRICES: &[(&str, TokenRates)] = &[
    ("gpt-5-3-codex", rates(1.75, 0.175, 0.0, 14.0)),
    ("gpt-5-4-nano", rates(0.20, 0.02, 0.0, 1.25)),
    ("gpt-5-6-luna", rates(0.20, 0.02, 0.25, 1.20)),
    ("gpt-5-6-sol", rates(4.0, 0.40, 5.0, 20.0)),
    ("gpt-5-6-terra", rates(2.0, 0.20, 2.50, 12.0)),
    ("gpt-6-astra", rates(10.0, 1.0, 12.50, 50.0)),
    ("gpt-6-luna", rates(0.10, 0.01, 0.125, 0.50)),
    ("gpt-6-sol", rates(2.0, 0.20, 2.50, 10.0)),
    ("gpt-6-1-sol", rates(2.0, 0.10, 2.50, 10.0)),
    ("gpt-6-1-terra", rates(1.0, 0.05, 1.25, 5.0)),
    ("claude-haiku-4-5", rates(1.0, 0.10, 1.25, 5.0)),
    ("claude-haiku-5-5", rates(0.10, 0.01, 0.125, 0.50)),
    ("claude-sonnet-4", rates(3.0, 0.30, 3.75, 15.0)),
    ("claude-sonnet-4-6", rates(3.0, 0.30, 3.75, 15.0)),
    ("claude-sonnet-5", rates(2.0, 0.20, 2.50, 10.0)),
    ("claude-sonnet-5-5", rates(2.0, 0.10, 2.50, 10.0)),
    ("claude-opus-4-8", rates(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-4-8-fast", rates(10.0, 1.0, 12.50, 50.0)),
    ("claude-opus-5", rates(5.0, 0.50, 6.25, 25.0)),
    ("claude-opus-5-5", rates(4.0, 0.20, 5.0, 20.0)),
    ("claude-fable-5", rates(10.0, 1.0, 12.50, 50.0)),
    ("claude-fable-5-1", rates(10.0, 0.25, 12.50, 50.0)),
    ("gemini-3-8-flash", rates(0.75, 0.075, 0.0, 3.75)),
    ("gemini-4-argon", rates(2.0, 0.10, 0.0, 10.0)),
    ("mai-code-1-1-flash", rates(0.20, 0.02, 0.0, 1.20)),
    ("grok-4-5", rates(2.0, 0.50, 0.0, 6.0)),
    ("grok-4-6", rates(2.0, 0.50, 0.0, 6.0)),
    ("grok-4-7", rates(2.0, 0.50, 0.0, 6.0)),
    ("kimi-k3", rates(3.0, 0.30, 0.0, 15.0)),
];

/// Exact match, so `claude-opus-4.8-fast` never takes the `claude-opus-4.8` price.
fn copilot_rates(bare_model: &str) -> Option<TokenRates> {
    let id = bare_model
        .strip_suffix(LONG_CONTEXT_SUFFIX)
        .unwrap_or(bare_model)
        .to_ascii_lowercase()
        .replace('.', "-");
    COPILOT_PRICES
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, rates)| *rates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_style_cache_is_subset_of_prompt() {
        let rates = TokenRates {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            cache_read_per_1m: 0.1,
            cache_write_per_1m: 1.0,
        };
        let cost = estimate_with_rates(rates, 100, 10, 80, 0);
        assert!((cost - 48.0 / PER_MILLION).abs() < 1e-12);
    }

    #[test]
    fn anthropic_style_cache_separate_from_input() {
        let rates = TokenRates {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            cache_read_per_1m: 0.1,
            cache_write_per_1m: 1.25,
        };
        let cost = estimate_with_rates(rates, 20, 10, 80, 5);
        let expected = (20.0 + 8.0 + 6.25 + 20.0) / PER_MILLION;
        assert!((cost - expected).abs() < 1e-12);
    }

    #[test]
    fn split_cached_input_handles_subset_and_separate_reports() {
        assert_eq!(split_cached_input(100, 80), (20, 80));
        assert_eq!(split_cached_input(20, 80), (20, 80));
        assert_eq!(split_cached_input(100, 0), (100, 0));
        assert_eq!(split_cached_input(0, 0), (0, 0));
    }

    #[test]
    fn builtin_matches_claude_sonnet() {
        let book = PricingBook::default();
        let rates = book.rates_for("llmgw/bedrock/claude-sonnet-5");
        assert!((rates.input_per_1m - 3.0).abs() < f64::EPSILON);
        assert!((rates.output_per_1m - 15.0).abs() < f64::EPSILON);
    }

    fn book_with(local: &[&str], copilot: &[&str]) -> PricingBook {
        PricingBook {
            local_profiles: local.iter().map(ToString::to_string).collect(),
            copilot_profiles: copilot.iter().map(ToString::to_string).collect(),
            ..PricingBook::default()
        }
    }

    #[test]
    fn copilot_profile_uses_published_price_for_dotted_dashed_and_long_context_ids() {
        let book = book_with(&[], &["copilot"]);
        for key in [
            "copilot/claude-opus-5.5",
            "copilot/claude-opus-5-5",
            "copilot/claude-opus-5-5[1m]",
        ] {
            let rates = book.rates_for(key);
            assert!((rates.input_per_1m - 4.0).abs() < f64::EPSILON, "{key}");
            assert!((rates.output_per_1m - 20.0).abs() < f64::EPSILON, "{key}");
            assert!(
                (rates.cache_read_per_1m - 0.20).abs() < f64::EPSILON,
                "{key}"
            );
        }
    }

    #[test]
    fn copilot_fast_variant_does_not_take_base_model_price() {
        let book = book_with(&[], &["copilot"]);
        let rates = book.rates_for("copilot/claude-opus-4.8-fast");
        assert!((rates.input_per_1m - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn copilot_table_ignored_for_other_profiles() {
        let book = book_with(&[], &["copilot"]);
        let rates = book.rates_for("llmgw/claude-opus-5.5");
        assert!((rates.input_per_1m - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn local_profile_is_free_even_for_cloud_looking_ids() {
        let mut book = book_with(&["llama_swap"], &[]);
        book.global = rates(1.0, 0.1, 1.0, 2.0);
        assert!(book.rates_for("llama_swap/claude-sonnet-distill").is_zero());
    }

    #[test]
    fn configured_model_price_beats_local_and_copilot_defaults() {
        let mut book = book_with(&["local"], &["copilot"]);
        book.by_model
            .insert("copilot/gpt-6-sol".into(), rates(9.0, 0.9, 9.0, 9.0));
        book.by_model
            .insert("local/qwen".into(), rates(1.0, 0.1, 1.0, 1.0));
        assert!((book.rates_for("copilot/gpt-6-sol").input_per_1m - 9.0).abs() < f64::EPSILON);
        assert!((book.rates_for("local/qwen").input_per_1m - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn listing_labels_copilot_price_local_free_and_skips_auto_and_unknown() {
        let book = book_with(&["llama_swap"], &["copilot"]);
        let mut models = vec![
            json!({"id": "copilot/claude-opus-5-5[1m]", "display_name": "claude-opus-5-5 · copilot"}),
            json!({"id": "copilot/claude-haiku-5-5", "display_name": "claude-haiku-5-5 · copilot"}),
            json!({"id": "llama_swap/qwen", "display_name": "qwen · llama-swap"}),
            json!({"id": "copilot/auto-intelligence", "display_name": "Auto (Intelligence) · copilot"}),
            json!({"id": "remote/mystery", "display_name": "mystery · remote"}),
        ];
        annotate_listing(&mut models, &book);
        let names: Vec<&str> = models
            .iter()
            .map(|model| model["display_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "claude-opus-5-5 · copilot · $4/$20",
                "claude-haiku-5-5 · copilot · $0.1/$0.5",
                "qwen · llama-swap · free",
                "Auto (Intelligence) · copilot",
                "mystery · remote",
            ]
        );
        assert_eq!(models[0][PRICE_FIELD]["output_usd_per_1m"], 20.0);
        assert_eq!(models[2][PRICE_FIELD]["input_usd_per_1m"], 0.0);
        assert!(models[3].get(PRICE_FIELD).is_none());
        assert!(models[4].get(PRICE_FIELD).is_none());
    }

    #[test]
    fn usd_trims_trailing_zeros() {
        assert_eq!(usd(4.0), "$4");
        assert_eq!(usd(1.75), "$1.75");
        assert_eq!(usd(0.125), "$0.125");
    }

    fn sample_usage_row(
        model: &str,
        tokens_in: u64,
        tokens_out: u64,
    ) -> crate::schemas::usage_row::UsageRow {
        crate::schemas::usage_row::UsageRow {
            ts_ms: 0,
            model: model.into(),
            profile: "test".into(),
            status: 200,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in,
            tokens_out,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
            error: None,
            client: None,
        }
    }

    #[test]
    fn apply_usage_costs_cloud_model_nonzero() {
        let book = PricingBook::default();
        let mut report = UsageReport {
            total_requests: 1,
            total_errors: 0,
            total_tokens_in: 1_000_000,
            total_tokens_out: 0,
            recent: vec![sample_usage_row("openai/gpt-4o", 1_000_000, 0)],
        };
        apply_usage_costs(&mut report, &book);
        assert!(report.recent[0].cost_usd > 0.0);
    }

    #[test]
    fn apply_usage_costs_local_model_zero() {
        let book = PricingBook::default();
        let mut report = UsageReport {
            total_requests: 1,
            total_errors: 0,
            total_tokens_in: 1_000_000,
            total_tokens_out: 0,
            recent: vec![sample_usage_row("vllm/qwen-32b", 1_000_000, 0)],
        };
        apply_usage_costs(&mut report, &book);
        assert!(report.recent[0].cost_usd.abs() < f64::EPSILON);
    }
}

//! Estimated USD cost from token counts.
//!
//! Rates are USD per 1M tokens. Lookup order for a qualified `profile/model`:
//! 1. Exact model override from env
//! 2. Bare model id override
//! 3. Profile default rates
//! 4. Built-in cloud model guesses (when nothing else is set)
//! 5. Global hub defaults
//! 6. $0 (local / unknown)

use std::collections::HashMap;

use serde::Serialize;

use crate::configs::{HubConfig, TokenRates};
use crate::schemas::usage_report::UsageReport;
use crate::services::stats::StatsSnapshot;

const PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, Clone, Default)]
pub struct PricingBook {
    pub global: TokenRates,
    pub by_profile: HashMap<String, TokenRates>,
    /// Keys may be qualified (`profile/model`) or bare model ids.
    pub by_model: HashMap<String, TokenRates>,
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
        };
        for profile in &config.profiles {
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
    // OpenAI: cached tokens are a subset of prompt_tokens.
    // Anthropic: cache_read is often reported separately from input_tokens.
    let (uncached_in, cached_in) = if cache_read > tokens_in {
        (tokens_in, cache_read)
    } else {
        (tokens_in.saturating_sub(cache_read), cache_read)
    };
    let input = uncached_in as f64 * rates.input_per_1m / PER_MILLION;
    let cached = cached_in as f64 * rates.cache_read_per_1m / PER_MILLION;
    let written = cache_write as f64 * rates.cache_write_per_1m / PER_MILLION;
    let output = tokens_out as f64 * rates.output_per_1m / PER_MILLION;
    input + cached + written + output
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
    fn builtin_matches_claude_sonnet() {
        let book = PricingBook::default();
        let rates = book.rates_for("llmgw/bedrock/claude-sonnet-5");
        assert!((rates.input_per_1m - 3.0).abs() < f64::EPSILON);
        assert!((rates.output_per_1m - 15.0).abs() < f64::EPSILON);
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
        }
    }

    #[test]
    fn apply_usage_costs__cloud_model_nonzero() {
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
    fn apply_usage_costs__local_model_zero() {
        let book = PricingBook::default();
        let mut report = UsageReport {
            total_requests: 1,
            total_errors: 0,
            total_tokens_in: 1_000_000,
            total_tokens_out: 0,
            recent: vec![sample_usage_row("vllm/qwen-32b", 1_000_000, 0)],
        };
        apply_usage_costs(&mut report, &book);
        assert_eq!(report.recent[0].cost_usd, 0.0);
    }
}

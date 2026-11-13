//! Per-model prices an upstream lists in its own `/v1/models` as `pricing.prompt` and
//! `pricing.completion`, in USD per token (Bifrost and `OpenRouter` list them this way).
//! `OpenRouter`'s `pricing.overrides[].min_prompt_tokens` entries become the long-context tier.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde_json::Value;

use crate::configs::{LongContextTier, PriceCard, TokenRates};

const PER_MILLION: f64 = 1_000_000.0;
/// Rates are kept to six decimals per 1M tokens, which drops float noise such as 2.0000000000000004.
const DECIMAL_SCALE: f64 = 1_000_000.0;

/// The last listed prices of every profile, keyed `profile/model`.
#[derive(Debug)]
pub struct ListedPrices {
    table: ArcSwap<HashMap<String, PriceCard>>,
}

impl Default for ListedPrices {
    fn default() -> Self {
        Self {
            table: ArcSwap::from_pointee(HashMap::new()),
        }
    }
}

impl ListedPrices {
    /// The stored prices. Empty until the first listing that carries any.
    pub fn snapshot(&self) -> Arc<HashMap<String, PriceCard>> {
        self.table.load_full()
    }

    /// Replaces `profile`'s entries with `rates`, keyed by the model id the upstream lists.
    pub fn replace_profile(&self, profile: &str, rates: &HashMap<String, PriceCard>) {
        let prefix = format!("{profile}/");
        self.table.rcu(|current| {
            let mut next: HashMap<String, PriceCard> = current
                .iter()
                .filter(|(key, _)| !key.starts_with(&prefix))
                .map(|(key, card)| (key.clone(), *card))
                .collect();
            next.extend(
                rates
                    .iter()
                    .map(|(model, card)| (format!("{prefix}{model}"), *card)),
            );
            next
        });
    }
}

/// Model id → price card for every listed entry that carries a non-zero base `pricing` block.
pub fn from_listing(items: &[&Value]) -> HashMap<String, PriceCard> {
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            let pricing = item.get("pricing")?;
            let base = rates_from(pricing, TokenRates::default());
            let card = PriceCard {
                base,
                long_context: long_context_tier(pricing, base),
            };
            (!base.is_zero()).then(|| (id.to_string(), card))
        })
        .collect()
}

/// The override with a prompt threshold, as one tier. An override without `min_prompt_tokens`
/// or with a `utc_start`/`utc_end` window is a timed promotion, so it is not a tier. The hub
/// applies a tier to prompts strictly above its threshold, so `min_prompt_tokens = N` becomes
/// `threshold = N - 1`. Only one tier fits a card, so the lowest threshold wins.
fn long_context_tier(pricing: &Value, base: TokenRates) -> Option<LongContextTier> {
    pricing
        .get("overrides")
        .and_then(Value::as_array)?
        .iter()
        .filter(|item| item.get("utc_start").is_none() && item.get("utc_end").is_none())
        .filter_map(|item| {
            let min = item.get("min_prompt_tokens").and_then(Value::as_u64)?;
            Some(LongContextTier {
                threshold_tokens: min.saturating_sub(1),
                rates: rates_from(item, base),
            })
        })
        .min_by_key(|tier| tier.threshold_tokens)
}

/// Reads the per-token fields of a price block as USD per 1M. A field the block lacks keeps the
/// matching value of `fallback`, so an override only restates the rates it changes.
fn rates_from(block: &Value, fallback: TokenRates) -> TokenRates {
    let per_1m = |field: &str, keep: f64| {
        block
            .get(field)
            .and_then(number)
            .map_or(keep, |per_token| round(per_token * PER_MILLION))
    };
    TokenRates {
        input_per_1m: per_1m("prompt", fallback.input_per_1m),
        output_per_1m: per_1m("completion", fallback.output_per_1m),
        cache_read_per_1m: per_1m("input_cache_read", fallback.cache_read_per_1m),
        cache_write_per_1m: per_1m("input_cache_write", fallback.cache_write_per_1m),
    }
}

/// A price is a JSON number or a decimal string.
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .filter(|price| price.is_finite() && *price >= 0.0)
}

fn round(value: f64) -> f64 {
    (value * DECIMAL_SCALE).round() / DECIMAL_SCALE
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "fixture prices are exact decimal constants"
)]
mod tests {
    use serde_json::json;

    use super::*;

    fn listing() -> Vec<Value> {
        vec![
            json!({"id": "azure/gpt-4.1-nano", "pricing": {
                "prompt": "0.0000001000", "completion": "0.0000004000",
                "input_cache_read": "0.0000000250", "web_search": "0.0140000000"}}),
            json!({"id": "azure/gpt-4.1", "pricing": {"prompt": 0.000_002, "completion": 0.000_008}}),
            json!({"id": "free-model", "pricing": {"prompt": "0", "completion": "0"}}),
            json!({"id": "no-pricing"}),
        ]
    }

    #[test]
    fn per_token_prices_become_usd_per_1m() {
        let items = listing();
        let refs: Vec<&Value> = items.iter().collect();

        let cards = from_listing(&refs);

        let nano = cards["azure/gpt-4.1-nano"].base;
        assert_eq!(nano.input_per_1m, 0.1);
        assert_eq!(nano.output_per_1m, 0.4);
        assert_eq!(nano.cache_read_per_1m, 0.025);
        assert_eq!(cards["azure/gpt-4.1"].base.input_per_1m, 2.0);
    }

    #[test]
    fn zero_or_missing_pricing_is_not_listed() {
        let items = listing();
        let refs: Vec<&Value> = items.iter().collect();

        let cards = from_listing(&refs);

        assert_eq!(cards.len(), 2);
        assert!(!cards.contains_key("free-model"));
        assert!(!cards.contains_key("no-pricing"));
    }

    #[test]
    fn openrouter_prompt_override_becomes_a_long_context_tier() {
        let item = json!({"id": "openai/gpt-x", "pricing": {
            "prompt": "0.000001", "completion": "0.000005",
            "input_cache_read": "0.00000005", "input_cache_write": "0.00000125",
            "overrides": [{"min_prompt_tokens": 272_000, "prompt": "0.000002",
                "completion": "0.0000075", "input_cache_read": "0.0000001",
                "input_cache_write": "0.0000025"}]}});

        let cards = from_listing(&[&item]);
        let card = cards["openai/gpt-x"];

        assert_eq!(card.base.input_per_1m, 1.0);
        let tier = card.long_context.expect("tier from the override");
        assert_eq!(tier.threshold_tokens, 271_999);
        assert_eq!(tier.rates.input_per_1m, 2.0);
        assert_eq!(tier.rates.output_per_1m, 7.5);
        assert_eq!(tier.rates.cache_read_per_1m, 0.1);
        assert_eq!(tier.rates.cache_write_per_1m, 2.5);
    }

    #[test]
    fn timed_and_partial_overrides_do_not_become_tiers() {
        let timed = json!({"id": "m/timed", "pricing": {"prompt": "0.000001",
            "overrides": [{"utc_start": 1500, "utc_end": 100, "prompt": "0.00000008"}]}});
        let partial = json!({"id": "m/partial", "pricing": {"prompt": "0.000001",
            "completion": "0.000004", "overrides": [{"min_prompt_tokens": 1000}]}});

        let cards = from_listing(&[&timed, &partial]);

        assert!(cards["m/timed"].long_context.is_none());
        let tier = cards["m/partial"].long_context.expect("tier");
        assert_eq!(tier.rates.input_per_1m, 1.0);
        assert_eq!(tier.rates.output_per_1m, 4.0);
    }

    #[test]
    fn replacing_a_profile_keeps_the_others() {
        let listed = ListedPrices::default();
        let card = PriceCard::flat(TokenRates {
            input_per_1m: 1.0,
            ..TokenRates::default()
        });
        listed.replace_profile("a", &HashMap::from([("m1".to_string(), card)]));
        listed.replace_profile("b", &HashMap::from([("m2".to_string(), card)]));

        listed.replace_profile("a", &HashMap::from([("m3".to_string(), card)]));

        let table = listed.snapshot();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["a/m3", "b/m2"]);
    }
}

use crate::configs::{LongContextTier, TokenRates};

/// A model's base rates, plus the long-context tier when one is published.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PriceCard {
    pub base: TokenRates,
    pub long_context: Option<LongContextTier>,
}

impl PriceCard {
    /// A card with base rates only.
    #[must_use]
    pub const fn flat(base: TokenRates) -> Self {
        Self {
            base,
            long_context: None,
        }
    }

    /// Fills missing cache rates on the base and on the tier.
    #[must_use]
    pub fn with_cache_defaults(self) -> Self {
        Self {
            base: self.base.with_cache_defaults(),
            long_context: self.long_context.map(LongContextTier::with_cache_defaults),
        }
    }

    /// The long-context tier, when the prompt is strictly above its threshold.
    #[must_use]
    pub fn tier_for_prompt(&self, prompt_tokens: u64) -> Option<LongContextTier> {
        self.long_context
            .filter(|tier| prompt_tokens > tier.threshold_tokens)
    }

    /// The tier rates above the threshold, else the base rates.
    #[must_use]
    pub fn rates_for_prompt(&self, prompt_tokens: u64) -> TokenRates {
        self.tier_for_prompt(prompt_tokens)
            .map_or(self.base, |tier| tier.rates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(input: f64, output: f64) -> TokenRates {
        TokenRates {
            input_per_1m: input,
            output_per_1m: output,
            cache_read_per_1m: 0.0,
            cache_write_per_1m: 0.0,
        }
    }

    #[test]
    fn card_without_a_tier_has_no_long_rates() {
        let card = PriceCard::flat(rates(1.0, 2.0));
        assert!(card.tier_for_prompt(u64::MAX).is_none());
        assert_eq!(card.rates_for_prompt(u64::MAX), rates(1.0, 2.0));
    }

    #[test]
    fn with_cache_defaults_applies_to_the_tier_as_well_as_the_base() {
        let card = PriceCard {
            base: rates(1.0, 2.0),
            long_context: Some(LongContextTier {
                threshold_tokens: 100,
                rates: rates(5.0, 6.0),
            }),
        }
        .with_cache_defaults();
        assert!((card.base.cache_read_per_1m - 0.1).abs() < f64::EPSILON);
        let tier = card.long_context.expect("tier kept");
        assert!((tier.rates.cache_read_per_1m - 0.5).abs() < f64::EPSILON);
        assert!((tier.rates.cache_write_per_1m - 5.0).abs() < f64::EPSILON);
    }
}

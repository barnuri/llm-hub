use crate::configs::TokenRates;

/// Rates that apply once a prompt is strictly above `threshold_tokens`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongContextTier {
    pub threshold_tokens: u64,
    pub rates: TokenRates,
}

impl LongContextTier {
    /// Fills missing cache rates on the tier.
    #[must_use]
    pub fn with_cache_defaults(self) -> Self {
        Self {
            rates: self.rates.with_cache_defaults(),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_cache_defaults_fills_the_tier_cache_rates() {
        let tier = LongContextTier {
            threshold_tokens: 100_000,
            rates: TokenRates {
                input_per_1m: 0.5,
                output_per_1m: 2.5,
                cache_read_per_1m: 0.0,
                cache_write_per_1m: 0.0,
            },
        }
        .with_cache_defaults();
        assert_eq!(tier.threshold_tokens, 100_000);
        assert!((tier.rates.cache_read_per_1m - 0.05).abs() < f64::EPSILON);
        assert!((tier.rates.cache_write_per_1m - 0.5).abs() < f64::EPSILON);
    }
}

use crate::configs::TokenRates;

/// One request as the keep-warm summary reads it.
pub(crate) struct WarmRequest {
    pub(crate) ts_ms: u64,
    pub(crate) ping: bool,
    pub(crate) ok: bool,
    pub(crate) cache_read: u64,
    pub(crate) ping_cost_usd: f64,
    /// USD saved per cache-read token: the cache write rate less the cache read rate.
    pub(crate) saving_per_token_usd: f64,
}

impl WarmRequest {
    const PER_MILLION: f64 = 1_000_000.0;

    pub(crate) fn new(
        ts_ms: u64,
        ping: bool,
        ok: bool,
        cache_read: u64,
        ping_cost_usd: f64,
        rates: TokenRates,
    ) -> Self {
        let rates = rates.with_cache_defaults();
        let saving_per_1m = (rates.cache_write_per_1m - rates.cache_read_per_1m).max(0.0);
        Self {
            ts_ms,
            ping,
            ok,
            cache_read,
            ping_cost_usd,
            saving_per_token_usd: saving_per_1m / Self::PER_MILLION,
        }
    }
}

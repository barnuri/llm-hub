use serde::Serialize;

/// Keep-warm pings and the cache writes their reads avoided. Pings are counted in cost, never as requests.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct KeepWarmSummary {
    pub keepwarm_pings: u64,
    pub keepwarm_cost_usd: f64,
    /// User requests that came back after an idle gap of at least five minutes, read the cache, and had a ping in the gap.
    pub idle_returns_cached: u64,
    /// Estimated USD the cache reads of those returns saved against rewriting the prompt.
    pub est_write_saved_usd: f64,
    /// False when the figures come from in-memory counters that reset at restart.
    pub persisted: bool,
}

impl KeepWarmSummary {
    pub fn absorb(&mut self, other: &KeepWarmSummary) {
        self.keepwarm_pings += other.keepwarm_pings;
        self.keepwarm_cost_usd += other.keepwarm_cost_usd;
        self.idle_returns_cached += other.idle_returns_cached;
        self.est_write_saved_usd += other.est_write_saved_usd;
    }
}

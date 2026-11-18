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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorb_adds_every_counter_and_keeps_its_own_persisted_flag() {
        let mut total = KeepWarmSummary {
            keepwarm_pings: 2,
            keepwarm_cost_usd: 0.5,
            idle_returns_cached: 1,
            est_write_saved_usd: 1.25,
            persisted: true,
        };
        let other = KeepWarmSummary {
            keepwarm_pings: 3,
            keepwarm_cost_usd: 0.25,
            idle_returns_cached: 4,
            est_write_saved_usd: 0.75,
            persisted: false,
        };

        total.absorb(&other);

        assert_eq!(
            total,
            KeepWarmSummary {
                keepwarm_pings: 5,
                keepwarm_cost_usd: 0.75,
                idle_returns_cached: 5,
                est_write_saved_usd: 2.0,
                persisted: true,
            }
        );
    }

    #[test]
    fn it_serializes_with_snake_case_field_names() {
        let value = serde_json::to_value(KeepWarmSummary {
            keepwarm_pings: 1,
            persisted: true,
            ..KeepWarmSummary::default()
        })
        .unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "keepwarm_pings": 1,
                "keepwarm_cost_usd": 0.0,
                "idle_returns_cached": 0,
                "est_write_saved_usd": 0.0,
                "persisted": true,
            })
        );
    }
}

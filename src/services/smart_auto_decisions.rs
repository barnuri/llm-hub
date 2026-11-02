//! A bounded in-memory log of Smart Auto routes, one entry per routed request.

use std::sync::{Mutex, PoisonError};

use crate::schemas::smart_auto_decision::SmartAutoDecision;
use crate::services::bounded_ring::BoundedRing;

/// The latest routes in a ring of at most 500 entries, oldest dropped first.
#[derive(Debug)]
pub struct SmartAutoDecisionLog {
    ring: Mutex<BoundedRing<SmartAutoDecision>>,
}

impl Default for SmartAutoDecisionLog {
    fn default() -> Self {
        Self {
            ring: Mutex::new(BoundedRing::new(Self::MAX_DECISIONS)),
        }
    }
}

impl SmartAutoDecisionLog {
    const MAX_DECISIONS: usize = 500;

    /// Appends `decision`, dropping the oldest entry when the ring is full.
    pub fn record(&self, decision: SmartAutoDecision) {
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        ring.push(decision);
    }

    /// Up to `limit` routes, newest first.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<SmartAutoDecision> {
        let ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        ring.iter().rev().take(limit).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::work_role::WorkRole;

    fn decision(ts_ms: u64) -> SmartAutoDecision {
        SmartAutoDecision {
            ts_ms,
            session: None,
            tier: "balance",
            role: WorkRole::Conversation,
            effort: Some("medium"),
            model: "alpha/m1".to_owned(),
            reason: "fresh",
        }
    }

    #[test]
    fn ring_keeps_only_the_newest_routes() {
        let log = SmartAutoDecisionLog::default();
        let max = SmartAutoDecisionLog::MAX_DECISIONS;
        let newest = u64::try_from(max).unwrap();
        for ts_ms in 0..=newest {
            log.record(decision(ts_ms));
        }

        let recent = log.recent(usize::MAX);

        assert_eq!(recent.len(), max);
        assert_eq!(recent[0].ts_ms, newest);
        assert_eq!(recent.last().map(|entry| entry.ts_ms), Some(1));
    }

    #[test]
    fn recent_honours_its_limit() {
        let log = SmartAutoDecisionLog::default();
        for ts_ms in 0..3 {
            log.record(decision(ts_ms));
        }

        let recent = log.recent(2);

        assert_eq!(
            recent.iter().map(|entry| entry.ts_ms).collect::<Vec<_>>(),
            [2, 1]
        );
    }
}

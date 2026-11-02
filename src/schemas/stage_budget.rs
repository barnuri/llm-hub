use std::time::Duration;

use crate::schemas::smart_auto_tier::SmartAutoTier;

/// The limits of one reviewed request: model calls, tokens, bytes and time across every stage.
/// The solver counts as the first call, so Balance allows a judge and one repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageBudget {
    calls_left: u8,
    tokens_left: u64,
    bytes_left: u64,
    /// The whole review's wall-clock limit, solver included.
    pub wall_time: Duration,
    /// The longest one stage may run before it is abandoned.
    pub stage_deadline: Duration,
}

impl StageBudget {
    /// Model calls Balance may make: the solver, a judge and one repair.
    pub const BALANCE_MAX_CALLS: u8 = 3;
    /// Model calls Intelligence may make: the solver, two critics and one repair.
    pub const INTELLIGENCE_MAX_CALLS: u8 = 4;
    const BALANCE_MAX_TOKENS: u64 = 64_000;
    const INTELLIGENCE_MAX_TOKENS: u64 = 128_000;
    const BALANCE_MAX_BYTES: u64 = 256 * 1024;
    const INTELLIGENCE_MAX_BYTES: u64 = 512 * 1024;
    const BALANCE_WALL_SECS: u64 = 120;
    const INTELLIGENCE_WALL_SECS: u64 = 240;
    const BALANCE_STAGE_SECS: u64 = 60;
    const INTELLIGENCE_STAGE_SECS: u64 = 90;

    /// The budget of a reviewed tier. Efficiency and Fast are never reviewed, so they have none.
    #[must_use]
    pub fn for_tier(tier: SmartAutoTier) -> Option<StageBudget> {
        let (calls, tokens, bytes, wall, stage) = match tier {
            SmartAutoTier::Balance => (
                Self::BALANCE_MAX_CALLS,
                Self::BALANCE_MAX_TOKENS,
                Self::BALANCE_MAX_BYTES,
                Self::BALANCE_WALL_SECS,
                Self::BALANCE_STAGE_SECS,
            ),
            SmartAutoTier::Intelligence => (
                Self::INTELLIGENCE_MAX_CALLS,
                Self::INTELLIGENCE_MAX_TOKENS,
                Self::INTELLIGENCE_MAX_BYTES,
                Self::INTELLIGENCE_WALL_SECS,
                Self::INTELLIGENCE_STAGE_SECS,
            ),
            SmartAutoTier::Efficiency | SmartAutoTier::Fast => return None,
        };
        Some(StageBudget {
            calls_left: calls,
            tokens_left: tokens,
            bytes_left: bytes,
            wall_time: Duration::from_secs(wall),
            stage_deadline: Duration::from_secs(stage),
        })
    }

    /// Takes one model call from the budget. Returns `false`, and takes nothing, once it is spent.
    pub fn admit_call(&mut self) -> bool {
        if self.is_exhausted() {
            return false;
        }
        self.calls_left -= 1;
        true
    }

    /// Charges the tokens and bytes a finished call used. Returns `false` once the budget is spent.
    pub fn charge(&mut self, tokens: u64, bytes: u64) -> bool {
        self.tokens_left = self.tokens_left.saturating_sub(tokens);
        self.bytes_left = self.bytes_left.saturating_sub(bytes);
        !self.is_exhausted()
    }

    /// True when no call, token or byte is left.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.calls_left == 0 || self.tokens_left == 0 || self.bytes_left == 0
    }

    /// Model calls still allowed.
    #[cfg(test)]
    #[must_use]
    pub fn calls_left(&self) -> u8 {
        self.calls_left
    }

    /// Tokens still allowed across the review.
    #[cfg(test)]
    #[must_use]
    pub fn tokens_left(&self) -> u64 {
        self.tokens_left
    }

    /// Reply bytes still allowed across the review.
    #[must_use]
    pub fn bytes_left(&self) -> u64 {
        self.bytes_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_allows_three_calls_and_intelligence_four() {
        let mut balance = StageBudget::for_tier(SmartAutoTier::Balance).unwrap();
        assert_eq!(balance.calls_left(), 3);
        assert!(balance.admit_call() && balance.admit_call() && balance.admit_call());
        assert!(!balance.admit_call());
        assert_eq!(balance.calls_left(), 0);

        let mut intelligence = StageBudget::for_tier(SmartAutoTier::Intelligence).unwrap();
        assert_eq!(intelligence.calls_left(), 4);
        for _ in 0..4 {
            assert!(intelligence.admit_call());
        }
        assert!(!intelligence.admit_call());
    }

    #[test]
    fn unreviewed_tiers_have_no_budget() {
        assert!(StageBudget::for_tier(SmartAutoTier::Efficiency).is_none());
        assert!(StageBudget::for_tier(SmartAutoTier::Fast).is_none());
    }

    #[test]
    fn tokens_and_bytes_exhaust_the_budget_without_going_negative() {
        let mut budget = StageBudget::for_tier(SmartAutoTier::Balance).unwrap();
        assert!(budget.charge(1_000, 1_000));
        assert!(!budget.charge(u64::MAX, 0));
        assert_eq!(budget.tokens_left(), 0);
        assert!(budget.is_exhausted());
        assert!(!budget.admit_call());

        let mut by_bytes = StageBudget::for_tier(SmartAutoTier::Balance).unwrap();
        assert!(!by_bytes.charge(0, u64::MAX));
        assert_eq!(by_bytes.bytes_left(), 0);
    }

    #[test]
    fn stage_deadline_is_inside_the_wall_time() {
        for tier in [SmartAutoTier::Balance, SmartAutoTier::Intelligence] {
            let budget = StageBudget::for_tier(tier).unwrap();
            assert!(budget.stage_deadline < budget.wall_time);
        }
    }
}

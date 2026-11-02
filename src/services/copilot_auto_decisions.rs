//! A bounded in-memory log of Copilot Auto routing outcomes, read by the UI and the status line.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde::Serialize;

use crate::services::copilot_auto::{AutoTier, AutoTurn};

const MAX_DECISIONS: usize = 500;
const MAX_SESSIONS: usize = 256;

/// One routed `<profile>/auto` request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutoDecision {
    pub ts_ms: u64,
    pub session: Option<String>,
    pub profile: String,
    /// Auto's own choice, as an advertised qualified id.
    pub auto_pick: String,
    /// The qualified id actually sent.
    pub model: String,
    pub swap_reason: Option<String>,
    pub complexity: Option<&'static str>,
    pub selection_reason: Option<String>,
    pub turn: AutoTurn,
    pub estimated_prompt_tokens: u64,
    /// The effort sent with `model`, as its published level name.
    pub effort: Option<String>,
}

/// A logged decision and the Auto tier it was routed on, serialized as one flat object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TieredDecision {
    #[serde(flatten)]
    pub decision: AutoDecision,
    pub tier: AutoTier,
}

/// A decision recorded without a tier was routed on the default tier.
impl From<AutoDecision> for TieredDecision {
    fn from(decision: AutoDecision) -> TieredDecision {
        TieredDecision {
            decision,
            tier: AutoTier::default(),
        }
    }
}

/// The latest decisions in a ring, plus the latest decision of each recent
/// session, so a quiet session still shows its model after the ring moves on.
#[derive(Debug, Default)]
pub struct AutoDecisionLog {
    state: Mutex<LogState>,
}

#[derive(Debug, Default)]
struct LogState {
    ring: VecDeque<TieredDecision>,
    latest_by_session: HashMap<String, TieredDecision>,
    session_order: VecDeque<String>,
}

impl AutoDecisionLog {
    /// Appends `decision`, dropping the oldest entry once the ring is full.
    pub fn record(&self, decision: impl Into<TieredDecision>) {
        let decision = decision.into();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(session) = decision.decision.session.clone() {
            state.remember_session(session, decision.clone());
        }
        if state.ring.len() >= MAX_DECISIONS {
            state.ring.pop_front();
        }
        state.ring.push_back(decision);
    }

    /// Up to `limit` decisions, newest first, for one session or for all of them.
    pub fn recent(&self, session: Option<&str>, limit: usize) -> Vec<TieredDecision> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(session) = session else {
            return state.ring.iter().rev().take(limit).cloned().collect();
        };
        let found: Vec<TieredDecision> = state
            .ring
            .iter()
            .rev()
            .filter(|entry| entry.decision.session.as_deref() == Some(session))
            .take(limit)
            .cloned()
            .collect();
        if !found.is_empty() || limit == 0 {
            return found;
        }
        state
            .latest_by_session
            .get(session)
            .cloned()
            .into_iter()
            .collect()
    }
}

impl LogState {
    /// Keeps `decision` as the session's latest; the least recently recorded session goes first.
    fn remember_session(&mut self, session: String, decision: TieredDecision) {
        if self
            .latest_by_session
            .insert(session.clone(), decision)
            .is_some()
        {
            self.session_order.retain(|known| *known != session);
        }
        while self.latest_by_session.len() > MAX_SESSIONS {
            let Some(oldest) = self.session_order.pop_front() else {
                break;
            };
            self.latest_by_session.remove(&oldest);
        }
        self.session_order.push_back(session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(session: Option<&str>, ts_ms: u64) -> AutoDecision {
        AutoDecision {
            ts_ms,
            session: session.map(str::to_string),
            profile: "copilot".to_string(),
            auto_pick: "copilot/claude-haiku-5-5".to_string(),
            model: "copilot/claude-sonnet-5-5".to_string(),
            swap_reason: None,
            complexity: Some("low"),
            selection_reason: None,
            turn: AutoTurn::Routed,
            estimated_prompt_tokens: 10,
            effort: None,
        }
    }

    fn timestamps(decisions: &[TieredDecision]) -> Vec<u64> {
        decisions.iter().map(|entry| entry.decision.ts_ms).collect()
    }

    #[test]
    fn entries_carry_the_tier_flat_and_default_to_balance() {
        let log = AutoDecisionLog::default();
        log.record(decision(Some("a"), 1));
        log.record(TieredDecision {
            decision: decision(Some("a"), 2),
            tier: AutoTier::Efficiency,
        });

        let recent = log.recent(Some("a"), 10);
        let json = serde_json::to_value(&recent).unwrap();

        assert_eq!(json[0]["tier"], "efficiency");
        assert_eq!(json[0]["ts_ms"], 2);
        assert_eq!(json[1]["tier"], "balance");
        assert_eq!(json[1]["session"], "a");
    }

    #[test]
    fn recent_is_newest_first_and_filters_by_session() {
        let log = AutoDecisionLog::default();
        log.record(decision(Some("a"), 1));
        log.record(decision(Some("b"), 2));
        log.record(decision(None, 3));
        log.record(decision(Some("a"), 4));

        assert_eq!(timestamps(&log.recent(None, 10)), [4, 3, 2, 1]);
        assert_eq!(timestamps(&log.recent(None, 2)), [4, 3]);
        assert_eq!(timestamps(&log.recent(Some("a"), 10)), [4, 1]);
        assert!(log.recent(Some("unknown"), 10).is_empty());
    }

    #[test]
    fn ring_is_bounded_and_a_session_keeps_its_latest_after_eviction() {
        let log = AutoDecisionLog::default();
        log.record(decision(Some("quiet"), 0));
        for ts_ms in 1..=MAX_DECISIONS as u64 {
            log.record(decision(Some("busy"), ts_ms));
        }

        let all = log.recent(None, MAX_DECISIONS + 10);

        assert_eq!(all.len(), MAX_DECISIONS);
        assert_eq!(all[0].decision.ts_ms, MAX_DECISIONS as u64);
        assert_eq!(timestamps(&log.recent(Some("quiet"), 10)), [0]);
    }

    #[test]
    fn session_latest_map_is_bounded_oldest_first() {
        let log = AutoDecisionLog::default();
        for index in 0..=MAX_SESSIONS as u64 {
            log.record(decision(Some(&index.to_string()), index));
        }
        let state = log.state.lock().unwrap();

        assert_eq!(state.latest_by_session.len(), MAX_SESSIONS);
        assert!(!state.latest_by_session.contains_key("0"));
        assert_eq!(state.session_order.len(), MAX_SESSIONS);
    }
}

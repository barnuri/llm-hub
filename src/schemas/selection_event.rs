use serde::{Deserialize, Serialize};

use crate::schemas::effort_level::EffortLevel;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::schemas::work_role::WorkRole;

/// One selection or dispatch outcome. It holds salted scope keys and metadata, never prompt, response or tool text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionEvent {
    /// Position in the log, assigned when the event is recorded.
    pub seq: u64,
    /// UTC time of recording, in Unix milliseconds.
    pub ts_ms: u64,
    /// Salted key of the parent workflow, when the caller names one.
    pub workflow: Option<String>,
    /// Salted key of the worker: its worker id, or its Claude Code session when it sends none.
    pub worker: Option<String>,
    /// Salted key of one dispatch attempt.
    pub attempt: Option<String>,
    /// Salted key of one user turn of a worker.
    pub turn: Option<String>,
    /// The selector the caller named, such as `hub/smart-auto-balance`.
    pub selector: Option<String>,
    /// The Smart Auto or Auto tier the request routed on.
    pub tier: Option<String>,
    /// The work role of the turn, when the route has one.
    pub role: Option<WorkRole>,
    /// The turn is plan-backed: its text references a plan, tasks file or skill. `None` on events that predate this field.
    #[serde(default)]
    pub plan_backed: Option<bool>,
    /// The model call this event belongs to.
    pub stage: SelectionStage,
    /// The qualified id of the model this event concerns.
    pub model: String,
    /// Where the dispatch stands at this event.
    pub state: SelectionState,
    /// The route's reason code, such as `pinned` or `fresh`.
    pub reason: Option<String>,
    /// The routing policy version that made the selection.
    pub policy_version: u32,
    /// The effort the caller asked for.
    pub requested_effort: Option<EffortLevel>,
    /// The effort the router chose for this model.
    pub selected_effort: Option<EffortLevel>,
    /// The effort written into the request body.
    pub sent_effort: Option<EffortLevel>,
    /// The effort the upstream confirmed; `None` because no upstream echoes it yet.
    pub confirmed_effort: Option<EffortLevel>,
    /// The upstream HTTP status, once the attempt has one.
    pub status: Option<u16>,
    /// Milliseconds from dispatch to this event, once the attempt has finished.
    pub latency_ms: Option<u64>,
    /// Prompt tokens the upstream reported, `None` when it reported none.
    pub tokens_in: Option<u64>,
    /// Completion tokens the upstream reported, `None` when it reported none.
    pub tokens_out: Option<u64>,
    /// Cache reads the upstream reported, `None` when it reported none.
    pub cache_read_tokens: Option<u64>,
    /// Cache writes the upstream reported, `None` when it reported none.
    pub cache_write_tokens: Option<u64>,
}

impl SelectionEvent {
    /// The version of the Smart Auto routing policy that made a selection.
    const POLICY_VERSION: u32 = 1;

    /// An event in `selection_state` for `model` with no scope keys or metrics; the log assigns `seq` and `ts_ms`.
    pub fn new(
        stage: SelectionStage,
        selection_state: SelectionState,
        model: impl Into<String>,
    ) -> SelectionEvent {
        SelectionEvent {
            seq: 0,
            ts_ms: 0,
            workflow: None,
            worker: None,
            attempt: None,
            turn: None,
            selector: None,
            tier: None,
            role: None,
            plan_backed: None,
            stage,
            model: model.into(),
            state: selection_state,
            reason: None,
            policy_version: Self::POLICY_VERSION,
            requested_effort: None,
            selected_effort: None,
            sent_effort: None,
            confirmed_effort: None,
            status: None,
            latency_ms: None,
            tokens_in: None,
            tokens_out: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
        }
    }

    /// A copy of this event moved to `state`, with every other field unchanged.
    #[must_use]
    pub fn in_state(&self, state: SelectionState) -> SelectionEvent {
        SelectionEvent {
            state,
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn sample() -> SelectionEvent {
        SelectionEvent::new(
            SelectionStage::Solver,
            SelectionState::Selected,
            "copilot/claude-opus-4-5",
        )
    }

    #[test]
    fn serialized_event_holds_only_scope_keys_and_metrics() {
        let json = serde_json::to_value(sample()).unwrap();
        let keys: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected: BTreeSet<&str> = [
            "attempt",
            "cache_read_tokens",
            "cache_write_tokens",
            "confirmed_effort",
            "latency_ms",
            "model",
            "plan_backed",
            "policy_version",
            "reason",
            "requested_effort",
            "role",
            "selected_effort",
            "selector",
            "sent_effort",
            "seq",
            "stage",
            "state",
            "status",
            "tier",
            "tokens_in",
            "tokens_out",
            "ts_ms",
            "turn",
            "worker",
            "workflow",
        ]
        .into_iter()
        .collect();

        assert_eq!(keys, expected);
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = SelectionEvent {
            seq: 7,
            ts_ms: 1_700_000_000_000,
            worker: Some("salted-worker".to_owned()),
            role: Some(WorkRole::Implementation),
            selected_effort: Some(EffortLevel::High),
            cache_read_tokens: Some(4_096),
            ..sample()
        };

        let json = serde_json::to_string(&event).unwrap();

        assert_eq!(
            serde_json::from_str::<SelectionEvent>(&json).unwrap(),
            event
        );
    }

    #[test]
    fn in_state_changes_only_the_state() {
        let event = SelectionEvent {
            worker: Some("salted-worker".to_owned()),
            attempt: Some("salted-attempt".to_owned()),
            ..sample()
        };

        let started = event.in_state(SelectionState::Started);

        assert_eq!(started.state, SelectionState::Started);
        assert_eq!(
            started,
            SelectionEvent {
                state: SelectionState::Started,
                ..event
            }
        );
    }
}

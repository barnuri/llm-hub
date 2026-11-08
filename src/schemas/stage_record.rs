use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;

/// One stage's place in a review, as the selection log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRecord {
    pub stage: SelectionStage,
    /// Qualified id of the model that ran the stage.
    pub model: String,
    pub state: SelectionState,
    /// Why the stage was rejected, skipped or cancelled.
    pub reason: Option<&'static str>,
    /// The tokens the stage's reply reported. All zero when the stage made no reply.
    pub usage: StageUsage,
}

/// The tokens one model call reported, split the way the selection log stores them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageUsage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl StageRecord {
    /// A copy of this record that carries `usage`.
    #[must_use]
    pub fn with_usage(mut self, usage: StageUsage) -> StageRecord {
        self.usage = usage;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_usage_keeps_the_rest_of_the_record() {
        let record = StageRecord {
            stage: SelectionStage::Judge,
            model: "copilot/gpt-5".to_owned(),
            state: SelectionState::Completed,
            reason: None,
            usage: StageUsage::default(),
        };
        let usage = StageUsage {
            tokens_in: 900,
            tokens_out: 40,
            cache_read: 500,
            cache_write: 0,
        };

        let stored = record.clone().with_usage(usage);

        assert_eq!(stored.usage, usage);
        assert_eq!(stored.state, record.state);
        assert_eq!(stored.model, record.model);
    }
}

use serde_json::Value;

use crate::schemas::model_id::ModelId;
use crate::schemas::stage_record::StageRecord;

/// What a request sends back, and how its review went.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewOutcome {
    /// The chat completion to send: the solver's own, or its repair. `Null` when aborted.
    pub completion: Value,
    /// The model that wrote `completion`.
    pub served: ModelId,
    pub stages: Vec<StageRecord>,
    /// Why the draft went out without a full review or repair, when it did.
    pub review_failed: Option<&'static str>,
    /// True when at least one verdict was read, or a repair was written.
    pub reviewed: bool,
    /// True when a reviewer rejected the draft, which is then not sent and gets no repair.
    pub rejected: bool,
    /// True when the caller dropped the review and every in-flight stage was cancelled.
    pub aborted: bool,
}

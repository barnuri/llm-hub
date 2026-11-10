use serde::Serialize;

use crate::schemas::session_cache_cause::SessionCacheCause;
use crate::schemas::session_compaction::SessionCompaction;
use crate::schemas::session_composition::SessionComposition;
use crate::schemas::session_prompt_point::SessionPromptPoint;
use crate::schemas::session_summary::SessionSummary;

/// One session with its prompt-size series, compaction points, cache-write causes and composition.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub series: Vec<SessionPromptPoint>,
    pub series_sampled: bool,
    pub compactions: Vec<SessionCompaction>,
    pub cache_causes: Vec<SessionCacheCause>,
    pub composition: Option<SessionComposition>,
}

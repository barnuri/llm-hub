use serde::Serialize;

use crate::schemas::session_cache_cause::SessionCacheCause;
use crate::schemas::session_summary::SessionSummary;

/// The session list for one range, with the worker-less cache-write bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionList {
    pub range: String,
    pub sessions: Vec<SessionSummary>,
    pub no_session_id: SessionCacheCause,
}

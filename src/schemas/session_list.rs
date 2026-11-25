use serde::Serialize;

use crate::schemas::session_cache_cause::SessionCacheCause;
use crate::schemas::session_fallback::SessionFallback;
use crate::schemas::session_stream_type::SessionStreamType;
use crate::schemas::session_summary::SessionSummary;

/// The session list for one range, with the worker-less cache-write bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionList {
    pub range: String,
    pub sessions: Vec<SessionSummary>,
    pub no_session_id: SessionCacheCause,
    /// The worker-less requests grouped by fallback session key, most cache-write cost first, at most 50.
    pub fallback_sessions: Vec<SessionFallback>,
    /// Spend of every listed request by stream label, most expensive first.
    pub stream_types: Vec<SessionStreamType>,
}

//! Extensions llm-hub always runs. They come first, so they see a request first and its
//! response last.

mod copilot_context_code;
mod local_not_ready;
mod payload_too_large;
mod upstream_limits;

use std::sync::Arc;

use crate::ext::Extension;
use copilot_context_code::CopilotContextCode;
use local_not_ready::LocalNotReady;
use payload_too_large::PayloadTooLarge;
use upstream_limits::UpstreamLimits;

/// Why a built-in failed an attempt that the status alone would not explain. The attempt loop
/// takes it from the request state and writes it to the request log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttemptNote(pub String);

pub(crate) fn builtins() -> Vec<Arc<dyn Extension>> {
    vec![
        Arc::new(CopilotContextCode),
        Arc::new(PayloadTooLarge),
        Arc::new(LocalNotReady),
        Arc::new(UpstreamLimits),
    ]
}

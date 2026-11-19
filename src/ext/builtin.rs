//! Extensions llm-hub always runs. They come first, so they see a request first and its
//! response last.

mod copilot_context_code;
mod payload_too_large;

use std::sync::Arc;

use crate::ext::Extension;
use copilot_context_code::CopilotContextCode;
use payload_too_large::PayloadTooLarge;

pub(crate) fn builtins() -> Vec<Arc<dyn Extension>> {
    vec![Arc::new(CopilotContextCode), Arc::new(PayloadTooLarge)]
}

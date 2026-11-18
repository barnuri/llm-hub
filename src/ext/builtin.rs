//! Extensions llm-hub always runs. They come first, so they see a request first and its
//! response last.

mod copilot_context_code;

use std::sync::Arc;

use crate::ext::Extension;
use copilot_context_code::CopilotContextCode;

pub(crate) fn builtins() -> Vec<Arc<dyn Extension>> {
    vec![Arc::new(CopilotContextCode)]
}

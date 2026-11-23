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

/// The phases each built-in runs in, by name. The UI shows them.
const BUILTIN_PHASES: [(&str, &[&str]); 4] = [
    ("copilot-context-code", &["response"]),
    ("payload-too-large", &["response"]),
    ("local-not-ready", &["response"]),
    ("upstream-limits", &["response"]),
];

/// The phases of the built-in `name`. `None` when no built-in has that name.
pub(crate) fn builtin_phases(name: &str) -> Option<&'static [&'static str]> {
    BUILTIN_PHASES
        .iter()
        .find(|(builtin, _)| *builtin == name)
        .map(|(_, phases)| *phases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_lists_its_phases() {
        for builtin in builtins() {
            assert!(
                builtin_phases(builtin.name()).is_some(),
                "{}",
                builtin.name()
            );
        }
        assert_eq!(BUILTIN_PHASES.len(), builtins().len());
        assert_eq!(builtin_phases("library"), None);
    }
}

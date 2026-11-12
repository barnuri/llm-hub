use std::collections::HashMap;

use crate::schemas::model_context::strip_1m_suffix;

/// What the hub measured about one model right now. A field is `None` when nothing is known,
/// and the score then falls back to the built-in seed or adds no penalty.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelSignal {
    /// 0 (dearest) to 10 (free), from the price book.
    pub cost: Option<u8>,
    /// 0 (slow) to 10 (fast), from the measured time to first byte.
    pub speed: Option<u8>,
    /// Share of recent attempts that failed on the model's side, 0 to 1.
    pub error_rate: Option<f64>,
    /// Quota left on the model's profile, 0 to 100.
    pub quota_remaining_pct: Option<f64>,
}

/// The live signals of every model in one request's pool, keyed by qualified id.
#[derive(Debug, Default)]
pub struct LiveSignals {
    by_model: HashMap<String, ModelSignal>,
}

impl LiveSignals {
    #[must_use]
    pub fn new(by_model: HashMap<String, ModelSignal>) -> LiveSignals {
        LiveSignals { by_model }
    }

    /// The signal of `qualified_id`, which may carry `[1m]`. Empty when the model has none.
    #[must_use]
    pub fn get(&self, qualified_id: &str) -> ModelSignal {
        self.by_model
            .get(strip_1m_suffix(qualified_id))
            .copied()
            .unwrap_or_default()
    }
}

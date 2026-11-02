use std::collections::HashMap;

use crate::schemas::model_context::strip_1m_suffix;
use crate::schemas::model_descriptor::ModelDescriptor;

/// Every concrete model's descriptor from the last model refresh, keyed by qualified id.
#[derive(Debug, Default)]
pub struct ModelDescriptorSnapshot {
    by_qualified_id: HashMap<String, ModelDescriptor>,
}

impl ModelDescriptorSnapshot {
    /// A later descriptor with the same qualified id replaces an earlier one.
    #[must_use]
    pub fn new(descriptors: Vec<ModelDescriptor>) -> ModelDescriptorSnapshot {
        let by_qualified_id = descriptors
            .into_iter()
            .map(|descriptor| (descriptor.qualified_id.clone(), descriptor))
            .collect();
        ModelDescriptorSnapshot { by_qualified_id }
    }

    /// `qualified_id` is `<profile>/<model>`; a `[1m]` alias is ignored.
    #[allow(dead_code)] // consumed by Smart Auto eligibility and admission
    #[must_use]
    pub fn get(&self, qualified_id: &str) -> Option<&ModelDescriptor> {
        self.by_qualified_id.get(strip_1m_suffix(qualified_id))
    }

    /// `model` is the bare id the hub advertises, e.g. `claude-opus-5-5`; a `[1m]` alias is ignored.
    #[allow(dead_code)] // consumed by Smart Auto eligibility and admission
    #[must_use]
    pub fn for_model(&self, profile: &str, model: &str) -> Option<&ModelDescriptor> {
        self.get(&format!("{profile}/{}", strip_1m_suffix(model)))
    }

    /// Every descriptor in the snapshot, in no fixed order.
    pub fn iter(&self) -> impl Iterator<Item = &ModelDescriptor> {
        self.by_qualified_id.values()
    }

    #[allow(dead_code)] // consumed by Smart Auto eligibility and admission
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_qualified_id.len()
    }
}

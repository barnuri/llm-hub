/// Capacities in tokens; each field is `None` when unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolLimits {
    /// Total context window.
    pub context: Option<u64>,
    /// Prompt capacity.
    pub input: Option<u64>,
    /// Completion capacity.
    pub output: Option<u64>,
}

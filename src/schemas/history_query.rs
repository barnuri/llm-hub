/// Which events `history` returns: those after `since_seq` that match each set scope key, oldest first.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// The scope key of the workflow to match, or `None` for any workflow.
    pub workflow: Option<String>,
    /// The scope key of the worker to match, or `None` for any worker.
    pub worker: Option<String>,
    pub since_seq: u64,
    pub limit: usize,
}

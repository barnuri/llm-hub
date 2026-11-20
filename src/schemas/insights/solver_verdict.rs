/// The verified outcome of one Smart Auto attempt, credited to the solver model that completed it.
///
/// `passed` is true for passing tests, a passing review and a user acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolverVerdict {
    pub model: String,
    /// Salted session key; `None` when the attempt had no worker.
    pub worker: Option<String>,
    pub passed: bool,
    pub ts_ms: u64,
}

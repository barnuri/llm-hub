/// Why a review stage produced no usable reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageError {
    /// The stage named a `hub/*` selector, which would recurse into Smart Auto.
    Recursion(String),
    /// The reply grew past the stage's byte budget.
    TooLarge,
    /// The upstream failed to answer, or answered with a non-2xx status.
    Upstream(String),
    /// The reply body was not a JSON chat completion.
    Malformed(String),
    /// The stage ran past its deadline.
    Timeout,
}

impl StageError {
    /// The `snake_case` reason a `review_failed` event carries.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            StageError::Recursion(_) => "recursion_rejected",
            StageError::TooLarge => "response_too_large",
            StageError::Upstream(_) => "upstream_error",
            StageError::Malformed(_) => "malformed_reply",
            StageError::Timeout => "timeout",
        }
    }
}

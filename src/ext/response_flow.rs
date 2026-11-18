/// What a response phase decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseFlow {
    /// Go on with the next extension, then deliver the response or try the next fallback as usual.
    Continue,
    /// Treat this response as failed and try the next model in the chain. On the last model the
    /// response is delivered.
    RetryNext,
}

/// The evidence a review reads: the latest user task, bounded upstream by the request signals.
#[derive(Debug, Clone, Copy)]
pub struct ReviewEvidence<'a> {
    /// The most recent real user text, `None` when the request carries none.
    pub task: Option<&'a str>,
}

use crate::services::stats::RequestOutcome;

/// The `complete` phase view of one recorded outcome.
#[derive(Debug)]
pub struct CompleteCtx<'a> {
    outcome: &'a RequestOutcome,
}

impl<'a> CompleteCtx<'a> {
    pub(crate) fn new(outcome: &'a RequestOutcome) -> CompleteCtx<'a> {
        CompleteCtx { outcome }
    }

    /// The profile that served the request.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.outcome.profile
    }

    /// The model that served the request, as `<profile>/<model>`.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.outcome.model_key
    }

    /// The upstream status, before any `response` phase change.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.outcome.status
    }

    /// Time from the request to the last byte, in milliseconds.
    #[must_use]
    pub fn latency_ms(&self) -> u64 {
        self.outcome.latency_ms
    }

    /// Time to the first upstream byte, for streamed replies.
    #[must_use]
    pub fn ttft_ms(&self) -> Option<u64> {
        self.outcome.ttft_ms
    }

    /// Prompt tokens the upstream reported.
    #[must_use]
    pub fn tokens_in(&self) -> u64 {
        self.outcome.tokens_in
    }

    /// Completion tokens the upstream reported.
    #[must_use]
    pub fn tokens_out(&self) -> u64 {
        self.outcome.tokens_out
    }

    /// A short failure reason when the status is 400 or higher.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.outcome.error.as_deref()
    }

    /// The caller signature, such as `claude-code`.
    #[must_use]
    pub fn client(&self) -> Option<&str> {
        self.outcome.client.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_outcome_fields() {
        let outcome = RequestOutcome {
            profile: "groq".to_owned(),
            model_key: "groq/llama".to_owned(),
            status: 429,
            latency_ms: 12,
            ttft_ms: Some(3),
            tokens_in: 5,
            tokens_out: 7,
            error: Some("rate limited".to_owned()),
            client: Some("codex".to_owned()),
            ..RequestOutcome::default()
        };

        let ctx = CompleteCtx::new(&outcome);

        assert_eq!(ctx.profile(), "groq");
        assert_eq!(ctx.model(), "groq/llama");
        assert_eq!(ctx.status(), 429);
        assert_eq!(ctx.latency_ms(), 12);
        assert_eq!(ctx.ttft_ms(), Some(3));
        assert_eq!(ctx.tokens_in(), 5);
        assert_eq!(ctx.tokens_out(), 7);
        assert_eq!(ctx.error(), Some("rate limited"));
        assert_eq!(ctx.client(), Some("codex"));
    }
}

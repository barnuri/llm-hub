use crate::ext::{AttemptCtx, CompleteCtx, Flow, RequestCtx, ResponseCtx, ResponseFlow};

/// Code that runs in llm-hub's request phases. Only `name` is required. Every phase method has a
/// no-op default.
///
/// Request and attempt phases run extensions in registration order, and the response phase in
/// reverse. The hub logs a panic in a method and goes on with that method's default result.
pub trait Extension: Send + Sync + 'static {
    /// A short name for logs.
    fn name(&self) -> &str;

    /// Once per `/v1/*` request, before the hub resolves the model and fallbacks. Sees the
    /// client's headers and body.
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Flow {
        Flow::Continue
    }

    /// Once per upstream attempt, the primary and each fallback. Sees the body sent, and the
    /// headers before the hub adds upstream auth.
    fn on_attempt(&self, _ctx: &mut AttemptCtx<'_>) -> Flow {
        Flow::Continue
    }

    /// Return true to have the hub read an error body (status 400 or higher) before
    /// [`Extension::on_response`], so [`ResponseCtx::error_body`] holds it.
    fn wants_error_body(&self, _ctx: &ResponseCtx<'_>) -> bool {
        false
    }

    /// Once per upstream reply, before the hub decides to deliver it or try the next fallback.
    fn on_response(&self, _ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        ResponseFlow::Continue
    }

    /// Each time the hub records an outcome: every failed attempt, the final reply and keep-warm
    /// pings. It can run before the next fallback. Keep it fast. Spawn a task for slow work.
    fn on_complete(&self, _ctx: &CompleteCtx<'_>) {}
}

use crate::ext::{AttemptCtx, CompleteCtx, Flow, RequestCtx, ResponseCtx, ResponseFlow};

/// Code that runs in llm-hub's request phases. Every method has a no-op default, so an extension
/// overrides only the phases it needs, and a phase added later does not break it.
///
/// Request and attempt phases run extensions in registration order. The response phase runs
/// them in reverse, so the first extension to see a request is the last to see its response.
/// A panic in a method is logged and the request continues as if the method returned the default.
pub trait Extension: Send + Sync + 'static {
    /// A short name for logs.
    fn name(&self) -> &str;

    /// Once per `/v1/*` request, before the model and fallbacks are resolved. Sees the client's
    /// headers and body as the client sent them.
    fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Flow {
        Flow::Continue
    }

    /// Once per upstream attempt, the primary and each fallback. Sees the exact headers and body
    /// sent to that upstream.
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

    /// Once the hub records a request's outcome. Runs off the response path. Keep it fast, and
    /// spawn a task for slow work.
    fn on_complete(&self, _ctx: &CompleteCtx<'_>) {}
}

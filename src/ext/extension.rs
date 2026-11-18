use crate::ext::{
    AttemptCtx, CompleteCtx, ConfigChangeCtx, Flow, HttpRequestCtx, HttpResponseCtx, RequestCtx,
    ResponseCtx, ResponseFlow,
};

/// Code that runs in llm-hub's request phases. Only `name` is required. Every phase method has a
/// no-op default.
///
/// Request and attempt phases run extensions in registration order, and the response phase in
/// reverse. The hub logs a panic in a method and goes on with that method's default result.
pub trait Extension: Send + Sync + 'static {
    /// A short name for logs.
    fn name(&self) -> &str;

    /// Once per request on every route, before the hub checks the master key. Sees the headers
    /// only. A rejection answers the client without calling the route.
    fn on_http_request(&self, _ctx: &mut HttpRequestCtx<'_>) -> Flow {
        Flow::Continue
    }

    /// Once per response on every route, as it leaves the hub. It runs for a rejection too.
    fn on_http_response(&self, _ctx: &mut HttpResponseCtx<'_>) {}

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

    /// Before an admin call saves a setting. A rejection answers the call and the hub saves
    /// nothing. Sees which setting and, for a profile or key, its name. It never sees a secret.
    fn on_config_change(&self, _ctx: &ConfigChangeCtx<'_>) -> Flow {
        Flow::Continue
    }

    /// After an admin call saved a setting.
    fn on_config_changed(&self, _ctx: &ConfigChangeCtx<'_>) {}

    /// Each time the hub records an outcome: every failed attempt, the final reply and keep-warm
    /// pings. It can run before the next fallback. Keep it fast. Spawn a task for slow work.
    fn on_complete(&self, _ctx: &CompleteCtx<'_>) {}
}

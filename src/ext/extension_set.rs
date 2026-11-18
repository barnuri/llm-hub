use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::ext::hooks::HookSet;
use crate::ext::{
    AttemptCtx, CompleteCtx, ConfigChangeCtx, Extension, Flow, HttpRequestCtx, HttpResponseCtx,
    RequestCtx, ResponseCtx, ResponseFlow,
};

/// The extensions of a running hub, in the order request phases run them.
#[derive(Clone, Default)]
pub(crate) struct ExtensionSet {
    extensions: Vec<Arc<dyn Extension>>,
    hooks: Option<Arc<HookSet>>,
}

impl ExtensionSet {
    pub(crate) fn new(extensions: Vec<Arc<dyn Extension>>) -> ExtensionSet {
        ExtensionSet {
            extensions,
            hooks: None,
        }
    }

    /// The set with HTTP hooks that run after the extensions in the `request` and `attempt`
    /// phases, and as `complete` hooks.
    pub(crate) fn with_hooks(mut self, hooks: Option<Arc<HookSet>>) -> ExtensionSet {
        self.hooks = hooks;
        self
    }

    pub(crate) fn hooks(&self) -> Option<&Arc<HookSet>> {
        self.hooks.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn names(&self) -> Vec<&str> {
        self.extensions.iter().map(|ext| ext.name()).collect()
    }

    /// Runs `on_http_request` in order. The first rejection stops the phase.
    pub(crate) fn http_request(&self, ctx: &mut HttpRequestCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "http_request", Flow::Continue, || {
                ext.on_http_request(ctx)
            });
            if flow != Flow::Continue {
                return flow;
            }
        }
        Flow::Continue
    }

    /// Runs `on_http_response` in reverse order.
    pub(crate) fn http_response(&self, ctx: &mut HttpResponseCtx<'_>) {
        for ext in self.extensions.iter().rev() {
            guarded(ext.as_ref(), "http_response", (), || {
                ext.on_http_response(ctx);
            });
        }
    }

    /// Runs `on_request` in order. The first rejection stops the phase.
    pub(crate) fn request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "request", Flow::Continue, || {
                ext.on_request(ctx)
            });
            if flow != Flow::Continue {
                return flow;
            }
        }
        Flow::Continue
    }

    /// Runs `on_attempt` in order. The first rejection stops the phase.
    pub(crate) fn attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "attempt", Flow::Continue, || {
                ext.on_attempt(ctx)
            });
            if flow != Flow::Continue {
                return flow;
            }
        }
        Flow::Continue
    }

    /// True when any extension wants the error body of this reply.
    pub(crate) fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        self.extensions.iter().any(|ext| {
            guarded(ext.as_ref(), "wants_error_body", false, || {
                ext.wants_error_body(ctx)
            })
        })
    }

    /// Runs `on_response` in reverse order. Every extension runs; any `RetryNext` wins.
    pub(crate) fn response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        let mut decided = ResponseFlow::Continue;
        for ext in self.extensions.iter().rev() {
            let flow = guarded(ext.as_ref(), "response", ResponseFlow::Continue, || {
                ext.on_response(ctx)
            });
            if flow == ResponseFlow::RetryNext {
                decided = ResponseFlow::RetryNext;
            }
        }
        decided
    }

    /// Runs `on_config_change` in order. The first rejection stops the phase.
    pub(crate) fn config_change(&self, ctx: &ConfigChangeCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "config_change", Flow::Continue, || {
                ext.on_config_change(ctx)
            });
            if flow != Flow::Continue {
                return flow;
            }
        }
        Flow::Continue
    }

    /// Runs `on_config_changed` in order.
    pub(crate) fn config_changed(&self, ctx: &ConfigChangeCtx<'_>) {
        for ext in &self.extensions {
            guarded(ext.as_ref(), "config_changed", (), || {
                ext.on_config_changed(ctx);
            });
        }
    }

    /// Runs `on_complete` in order.
    pub(crate) fn complete(&self, ctx: &CompleteCtx<'_>) {
        for ext in &self.extensions {
            guarded(ext.as_ref(), "complete", (), || ext.on_complete(ctx));
        }
        if let Some(hooks) = &self.hooks {
            hooks.complete(ctx);
        }
    }
}

/// Runs one extension method. A panic is logged and replaced by `fallback`.
fn guarded<T>(ext: &dyn Extension, phase: &str, fallback: T, call: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|_| {
        tracing::error!(extension = ext.name(), phase, "extension panicked, skipped");
        fallback
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::http::{Extensions, HeaderMap, Method, StatusCode};
    use bytes::Bytes;

    use super::*;
    use crate::ext::attempt_ctx::AttemptTarget;
    use crate::ext::response_ctx::ResponseTarget;
    use crate::ext::{Api, ConfigKind};
    use crate::services::stats::RequestOutcome;

    /// Records every call into a shared log, and answers with fixed flows.
    struct Probe {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        request: Flow,
        response: ResponseFlow,
        wants_body: bool,
        panics: bool,
    }

    impl Probe {
        fn new(name: &'static str, log: &Arc<Mutex<Vec<String>>>) -> Probe {
            Probe {
                name,
                log: Arc::clone(log),
                request: Flow::Continue,
                response: ResponseFlow::Continue,
                wants_body: false,
                panics: false,
            }
        }

        fn note(&self, phase: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:{phase}", self.name));
            assert!(!self.panics, "probe panic");
        }
    }

    impl Extension for Probe {
        fn name(&self) -> &'static str {
            self.name
        }

        fn on_http_request(&self, _ctx: &mut HttpRequestCtx<'_>) -> Flow {
            self.note("http_request");
            self.request.clone()
        }

        fn on_http_response(&self, _ctx: &mut HttpResponseCtx<'_>) {
            self.note("http_response");
        }

        fn on_config_change(&self, _ctx: &ConfigChangeCtx<'_>) -> Flow {
            self.note("config_change");
            self.request.clone()
        }

        fn on_config_changed(&self, _ctx: &ConfigChangeCtx<'_>) {
            self.note("config_changed");
        }

        fn on_request(&self, _ctx: &mut RequestCtx<'_>) -> Flow {
            self.note("request");
            self.request.clone()
        }

        fn on_attempt(&self, _ctx: &mut AttemptCtx<'_>) -> Flow {
            self.note("attempt");
            self.request.clone()
        }

        fn wants_error_body(&self, _ctx: &ResponseCtx<'_>) -> bool {
            self.note("wants");
            self.wants_body
        }

        fn on_response(&self, _ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
            self.note("response");
            self.response
        }

        fn on_complete(&self, _ctx: &CompleteCtx<'_>) {
            self.note("complete");
        }
    }

    struct Silent;

    impl Extension for Silent {
        fn name(&self) -> &'static str {
            "silent"
        }
    }

    fn set(probes: Vec<Probe>) -> ExtensionSet {
        ExtensionSet::new(
            probes
                .into_iter()
                .map(|probe| Arc::new(probe) as Arc<dyn Extension>)
                .collect(),
        )
    }

    fn run_request(set: &ExtensionSet) -> Flow {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let mut ctx = RequestCtx::new(
            &Method::POST,
            "/v1/chat/completions",
            &headers,
            Bytes::new(),
            &mut state,
        );
        set.request(&mut ctx)
    }

    fn run_attempt(set: &ExtensionSet) -> Flow {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = AttemptTarget {
            profile: "p",
            model: "p/m",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        };
        let mut ctx = AttemptCtx::new(target, &headers, Bytes::new(), &mut state);
        set.attempt(&mut ctx)
    }

    fn run_response(set: &ExtensionSet) -> (bool, ResponseFlow) {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "p",
            model: "p/m",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            copilot: false,
            last_attempt: false,
        };
        let mut ctx = ResponseCtx::new(target, StatusCode::BAD_REQUEST, &mut headers, &mut state);
        (set.wants_error_body(&ctx), set.response(&mut ctx))
    }

    #[test]
    fn request_and_attempt_run_in_order_and_response_in_reverse() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let set = set(vec![Probe::new("a", &log), Probe::new("b", &log)]);

        run_request(&set);
        run_attempt(&set);
        run_response(&set);
        set.complete(&CompleteCtx::new(&RequestOutcome::default(), None));

        assert_eq!(
            *log.lock().unwrap(),
            [
                "a:request",
                "b:request",
                "a:attempt",
                "b:attempt",
                "a:wants",
                "b:wants",
                "b:response",
                "a:response",
                "a:complete",
                "b:complete"
            ]
        );
        assert_eq!(set.names(), ["a", "b"]);
    }

    fn run_http(set: &ExtensionSet) -> Flow {
        let mut headers = HeaderMap::new();
        let flow = set.http_request(&mut HttpRequestCtx::new(
            &Method::GET,
            "/healthz",
            &mut headers,
        ));
        let mut ctx = HttpResponseCtx::new(&Method::GET, "/healthz", StatusCode::OK, &mut headers);
        set.http_response(&mut ctx);
        flow
    }

    #[test]
    fn config_phases_run_in_order_and_a_rejection_stops_only_the_first() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut gate = Probe::new("gate", &log);
        gate.request = Flow::reject(403, "frozen");
        let set = set(vec![Probe::new("a", &log), gate, Probe::new("b", &log)]);
        let ctx = ConfigChangeCtx::new(ConfigKind::KeepWarm, None);

        assert_eq!(set.config_change(&ctx), Flow::reject(403, "frozen"));
        set.config_changed(&ctx);

        assert_eq!(
            *log.lock().unwrap(),
            [
                "a:config_change",
                "gate:config_change",
                "a:config_changed",
                "gate:config_changed",
                "b:config_changed"
            ]
        );
    }

    #[test]
    fn http_request_runs_in_order_and_http_response_in_reverse() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let set = set(vec![Probe::new("a", &log), Probe::new("b", &log)]);

        assert_eq!(run_http(&set), Flow::Continue);

        assert_eq!(
            *log.lock().unwrap(),
            [
                "a:http_request",
                "b:http_request",
                "b:http_response",
                "a:http_response"
            ]
        );
    }

    #[test]
    fn a_panic_in_an_http_phase_is_skipped_and_a_rejection_stops_http_request() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut broken = Probe::new("broken", &log);
        broken.panics = true;
        let mut gate = Probe::new("gate", &log);
        gate.request = Flow::reject(403, "no");
        let set = set(vec![broken, gate, Probe::new("after", &log)]);

        assert_eq!(run_http(&set), Flow::reject(403, "no"));

        let calls = log.lock().unwrap().clone();
        assert!(calls.contains(&"gate:http_request".to_owned()));
        assert!(!calls.contains(&"after:http_request".to_owned()));
        assert!(calls.contains(&"after:http_response".to_owned()));
    }

    #[test]
    fn the_first_rejection_stops_the_request_and_attempt_phases() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut gate = Probe::new("gate", &log);
        gate.request = Flow::reject(403, "no");
        let set = set(vec![gate, Probe::new("after", &log)]);

        assert_eq!(run_request(&set), Flow::reject(403, "no"));
        assert_eq!(run_attempt(&set), Flow::reject(403, "no"));

        assert_eq!(*log.lock().unwrap(), ["gate:request", "gate:attempt"]);
    }

    #[test]
    fn any_retry_next_wins_and_every_response_method_still_runs() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut retry = Probe::new("retry", &log);
        retry.response = ResponseFlow::RetryNext;
        retry.wants_body = true;
        let set = set(vec![retry, Probe::new("other", &log)]);

        assert_eq!(run_response(&set), (true, ResponseFlow::RetryNext));

        assert_eq!(
            *log.lock().unwrap(),
            ["retry:wants", "other:response", "retry:response"]
        );
    }

    #[test]
    fn a_panicking_extension_is_skipped_and_the_others_run() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut broken = Probe::new("broken", &log);
        broken.panics = true;
        broken.request = Flow::reject(500, "unreachable");
        broken.wants_body = true;
        let set = set(vec![broken, Probe::new("ok", &log)]);

        assert_eq!(run_request(&set), Flow::Continue);
        assert_eq!(run_response(&set), (false, ResponseFlow::Continue));
        set.complete(&CompleteCtx::new(&RequestOutcome::default(), None));

        assert!(log.lock().unwrap().contains(&"ok:complete".to_owned()));
    }

    #[test]
    fn default_methods_change_nothing() {
        let set = ExtensionSet::new(vec![Arc::new(Silent)]);

        assert_eq!(run_request(&set), Flow::Continue);
        assert_eq!(run_attempt(&set), Flow::Continue);
        assert_eq!(run_response(&set), (false, ResponseFlow::Continue));
        set.complete(&CompleteCtx::new(&RequestOutcome::default(), None));
        assert!(ExtensionSet::default().names().is_empty());
    }
}

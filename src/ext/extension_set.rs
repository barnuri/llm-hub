use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::ext::builtin::builtin_phases;
use crate::ext::hooks::HookSet;
use crate::ext::rules::RulesExtension;
use crate::ext::{
    AttemptCtx, CompleteCtx, ConfigChangeCtx, Extension, Flow, HttpRequestCtx, HttpResponseCtx,
    Outbox, RequestCtx, ResponseCtx, ResponseFlow, deliver,
};
use crate::schemas::extension_status::ExtensionKind;
use crate::services::extension_toggles::{ExtensionToggles, HOOKS_NAME};
use crate::services::notifications::dispatch::Notifier;

/// The extensions of a running hub, in the order request phases run them.
#[derive(Clone, Default)]
pub(crate) struct ExtensionSet {
    extensions: Vec<Arc<dyn Extension>>,
    hooks: Option<Arc<HookSet>>,
    /// Receives what extensions send with `ctx.notify(..)`. Without it those notifications drop.
    notifier: Option<Arc<Notifier>>,
    /// The rules file's extension, also last in `extensions`. `None` in a filtered set.
    rules: Option<Arc<RulesExtension>>,
}

impl ExtensionSet {
    pub(crate) fn new(extensions: Vec<Arc<dyn Extension>>) -> ExtensionSet {
        ExtensionSet {
            extensions,
            hooks: None,
            notifier: None,
            rules: None,
        }
    }

    /// The set with the rules file's extension added last.
    pub(crate) fn with_rules(mut self, rules: Option<RulesExtension>) -> ExtensionSet {
        if let Some(rules) = rules {
            let rules = Arc::new(rules);
            self.extensions
                .push(Arc::clone(&rules) as Arc<dyn Extension>);
            self.rules = Some(rules);
        }
        self
    }

    /// The set with HTTP hooks that run after the extensions in the `request` and `attempt`
    /// phases, and as `complete` hooks.
    pub(crate) fn with_hooks(mut self, hooks: Option<Arc<HookSet>>) -> ExtensionSet {
        self.hooks = hooks;
        self
    }

    /// The set with the notifier that stores what extensions send with `ctx.notify(..)`.
    pub(crate) fn with_notifier(mut self, notifier: Arc<Notifier>) -> ExtensionSet {
        self.notifier = Some(notifier);
        self
    }

    pub(crate) fn hooks(&self) -> Option<&Arc<HookSet>> {
        self.hooks.as_ref()
    }

    /// The rules file's extension, with every rule. `None` without a rules file and in a filtered set.
    pub(crate) fn rules(&self) -> Option<&RulesExtension> {
        self.rules.as_deref()
    }

    pub(crate) fn names(&self) -> Vec<&str> {
        self.extensions.iter().map(|ext| ext.name()).collect()
    }

    /// The name and kind of every extension, in run order. The hooks are not in it.
    pub(crate) fn kinds(&self) -> Vec<(&str, ExtensionKind)> {
        self.extensions
            .iter()
            .map(|ext| {
                let kind = if self.is_rules(ext) {
                    ExtensionKind::Rules
                } else if builtin_phases(ext.name()).is_some() {
                    ExtensionKind::Builtin
                } else {
                    ExtensionKind::Library
                };
                (ext.name(), kind)
            })
            .collect()
    }

    /// The set without what `toggles` turns off. Rules turned off leave the rules extension, and
    /// a set with nothing off keeps every extension as it is.
    pub(crate) fn filtered(&self, toggles: &ExtensionToggles) -> ExtensionSet {
        if toggles.is_empty() {
            return self.clone();
        }
        let mut extensions = Vec::with_capacity(self.extensions.len());
        for ext in &self.extensions {
            if toggles.extension(ext.name()).is_some() {
                continue;
            }
            let reduced = self
                .rules
                .as_ref()
                .filter(|_| self.is_rules(ext))
                .and_then(|rules| rules.without(|rule| toggles.rule(rule).is_some()));
            match reduced {
                Some(reduced) => extensions.push(Arc::new(reduced) as Arc<dyn Extension>),
                None => extensions.push(Arc::clone(ext)),
            }
        }
        ExtensionSet {
            extensions,
            hooks: self
                .hooks
                .as_ref()
                .filter(|_| toggles.extension(HOOKS_NAME).is_none())
                .map(Arc::clone),
            notifier: self.notifier.as_ref().map(Arc::clone),
            rules: None,
        }
    }

    /// True when `ext` is the rules file's extension.
    fn is_rules(&self, ext: &Arc<dyn Extension>) -> bool {
        self.rules
            .as_ref()
            .is_some_and(|rules| std::ptr::addr_eq(Arc::as_ptr(ext), Arc::as_ptr(rules)))
    }

    /// Runs `on_http_request` in order. The first rejection stops the phase.
    pub(crate) fn http_request(&self, ctx: &mut HttpRequestCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "http_request", Flow::Continue, || {
                ext.on_http_request(ctx)
            });
            self.flush(ext.as_ref(), ctx.outbox());
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
            self.flush(ext.as_ref(), ctx.outbox());
        }
    }

    /// Runs `on_request` in order. The first rejection stops the phase.
    pub(crate) fn request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        for ext in &self.extensions {
            let flow = guarded(ext.as_ref(), "request", Flow::Continue, || {
                ext.on_request(ctx)
            });
            self.flush(ext.as_ref(), ctx.outbox());
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
            self.flush(ext.as_ref(), ctx.outbox());
            if flow != Flow::Continue {
                return flow;
            }
        }
        Flow::Continue
    }

    /// True when any extension wants the error body of this reply.
    pub(crate) fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        self.extensions.iter().any(|ext| {
            let wants = guarded(ext.as_ref(), "wants_error_body", false, || {
                ext.wants_error_body(ctx)
            });
            self.flush(ext.as_ref(), ctx.outbox());
            wants
        })
    }

    /// Runs `on_response` in reverse order. Every extension runs; any `RetryNext` wins.
    pub(crate) fn response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        let mut decided = ResponseFlow::Continue;
        for ext in self.extensions.iter().rev() {
            let flow = guarded(ext.as_ref(), "response", ResponseFlow::Continue, || {
                ext.on_response(ctx)
            });
            self.flush(ext.as_ref(), ctx.outbox());
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
            self.flush(ext.as_ref(), ctx.outbox());
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
            self.flush(ext.as_ref(), ctx.outbox());
        }
    }

    /// Runs `on_complete` in order.
    pub(crate) fn complete(&self, ctx: &CompleteCtx<'_>) {
        for ext in &self.extensions {
            guarded(ext.as_ref(), "complete", (), || ext.on_complete(ctx));
            self.flush(ext.as_ref(), ctx.outbox());
        }
        if let Some(hooks) = &self.hooks {
            hooks.complete(ctx);
        }
    }

    /// Hands what `ext` sent with `ctx.notify(..)` in its last call to the notifier.
    fn flush(&self, ext: &dyn Extension, outbox: &Outbox) {
        deliver(self.notifier.as_deref(), ext.name(), outbox);
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
    use crate::configs::NotifyConfig;
    use crate::ext::attempt_ctx::AttemptTarget;
    use crate::ext::response_ctx::ResponseTarget;
    use crate::ext::{Api, ConfigKind, ExtNotification, ExtSeverity};
    use crate::schemas::notification::{
        NotificationKind, NotificationSeverity, NotificationStatus,
    };
    use crate::services::extension_toggles::sample_set;
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

    /// Sends one notification titled after the phase from every phase method.
    struct Notifying;

    impl Extension for Notifying {
        fn name(&self) -> &'static str {
            "notifying"
        }

        fn on_http_request(&self, ctx: &mut HttpRequestCtx<'_>) -> Flow {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "http_request", ""));
            Flow::Continue
        }

        fn on_http_response(&self, ctx: &mut HttpResponseCtx<'_>) {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "http_response", ""));
        }

        fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "request", ""));
            Flow::Continue
        }

        fn on_attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "attempt", ""));
            Flow::Continue
        }

        fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "wants", ""));
            false
        }

        fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "response", ""));
            ResponseFlow::Continue
        }

        fn on_config_change(&self, ctx: &ConfigChangeCtx<'_>) -> Flow {
            ctx.notify(ExtNotification::new(ExtSeverity::Info, "config_change", ""));
            Flow::Continue
        }

        fn on_config_changed(&self, ctx: &ConfigChangeCtx<'_>) {
            ctx.notify(ExtNotification::new(
                ExtSeverity::Info,
                "config_changed",
                "",
            ));
        }

        fn on_complete(&self, ctx: &CompleteCtx<'_>) {
            ctx.notify(
                ExtNotification::new(ExtSeverity::Critical, "complete", "body")
                    .with_model("p/m")
                    .with_profile("p"),
            );
        }
    }

    fn run_every_phase(set: &ExtensionSet) {
        run_http(set);
        run_request(set);
        run_attempt(set);
        run_response(set);
        let ctx = ConfigChangeCtx::new(ConfigKind::KeepWarm, None);
        set.config_change(&ctx);
        set.config_changed(&ctx);
        set.complete(&CompleteCtx::new(&RequestOutcome::default(), None));
    }

    fn notifier(enabled: bool) -> Arc<Notifier> {
        let config = NotifyConfig {
            enabled,
            ..NotifyConfig::default()
        };
        Notifier::new(&config, None)
    }

    #[test]
    fn a_notify_call_in_any_phase_lands_in_the_inbox_from_the_extension() {
        let notifier = notifier(true);
        let set = ExtensionSet::new(vec![Arc::new(Notifying)]).with_notifier(Arc::clone(&notifier));

        run_every_phase(&set);

        let listed = notifier.list();
        let mut titles: Vec<&str> = listed.iter().map(|n| n.title.as_str()).collect();
        titles.sort_unstable();
        assert_eq!(
            titles,
            [
                "attempt",
                "complete",
                "config_change",
                "config_changed",
                "http_request",
                "http_response",
                "request",
                "response",
                "wants"
            ]
        );
        assert!(listed.iter().all(|n| n.source == "ext:notifying"
            && n.kind == NotificationKind::Custom
            && n.status == NotificationStatus::Pending));
        let complete = listed.iter().find(|n| n.title == "complete").unwrap();
        assert_eq!(complete.severity, NotificationSeverity::Critical);
        assert_eq!(complete.body, "body");
        assert_eq!(complete.model.as_deref(), Some("p/m"));
        assert_eq!(complete.profile.as_deref(), Some("p"));
    }

    #[test]
    fn disabled_notifications_store_nothing() {
        let notifier = notifier(false);
        let set = ExtensionSet::new(vec![Arc::new(Notifying)]).with_notifier(Arc::clone(&notifier));

        run_every_phase(&set);

        assert!(notifier.list().is_empty());
    }

    #[test]
    fn without_a_notifier_notify_calls_are_dropped() {
        let set = ExtensionSet::new(vec![Arc::new(Notifying)]);

        run_every_phase(&set);

        assert!(set.notifier.is_none());
    }

    fn toggles(env: &[&str]) -> ExtensionToggles {
        let env: Vec<String> = env.iter().map(|entry| (*entry).to_owned()).collect();
        ExtensionToggles::load(&env, None)
    }

    #[test]
    fn nothing_off_keeps_every_extension_and_the_hooks_as_they_are() {
        let full = sample_set();

        let running = full.filtered(&toggles(&[]));

        assert_eq!(running.extensions.len(), full.extensions.len());
        assert!(
            running
                .extensions
                .iter()
                .zip(&full.extensions)
                .all(|(kept, original)| Arc::ptr_eq(kept, original))
        );
        assert!(Arc::ptr_eq(running.hooks().unwrap(), full.hooks().unwrap()));
        assert_eq!(running.rules().unwrap().len(), 2);
    }

    #[test]
    fn a_filter_removes_builtins_rules_and_hooks_and_keeps_the_full_set() {
        let full = sample_set().with_notifier(notifier(true));

        let running = full.filtered(&toggles(&["upstream-limits", "hooks", "rules:tag"]));

        assert_eq!(
            running.names(),
            [
                "copilot-context-code",
                "payload-too-large",
                "local-not-ready",
                "team-gate",
                "rules"
            ]
        );
        assert!(running.hooks().is_none());
        assert!(running.notifier.is_some());
        assert!(running.rules().is_none());
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
        running.attempt(&mut ctx);
        assert!(ctx.into_parts().0.is_none(), "the rule `tag` is off");

        let restored = full.filtered(&toggles(&[]));
        assert_eq!(restored.names().len(), 6);
        assert_eq!(restored.rules().unwrap().len(), 2);
        assert!(restored.hooks().is_some());
    }

    #[test]
    fn turning_the_rules_off_removes_the_rules_extension() {
        let running = sample_set().filtered(&toggles(&["rules", "team-*"]));

        assert_eq!(running.names().len(), 4);
        assert!(running.hooks().is_some());
    }

    #[test]
    fn a_rule_filter_that_names_no_rule_keeps_the_rules_extension() {
        let full = sample_set();

        let running = full.filtered(&toggles(&["rules:nope"]));

        assert!(Arc::ptr_eq(
            running.extensions.last().unwrap(),
            full.extensions.last().unwrap()
        ));
    }

    #[test]
    fn kinds_name_builtins_the_rules_and_library_extensions() {
        let set = sample_set();
        let kinds = set.kinds();

        assert_eq!(kinds[0], ("copilot-context-code", ExtensionKind::Builtin));
        assert_eq!(kinds[4], ("team-gate", ExtensionKind::Library));
        assert_eq!(kinds[5], ("rules", ExtensionKind::Rules));
        assert_eq!(
            ExtensionSet::new(vec![Arc::new(Silent)]).kinds(),
            [("silent", ExtensionKind::Library)]
        );
        assert!(ExtensionSet::default().with_rules(None).rules().is_none());
    }
}

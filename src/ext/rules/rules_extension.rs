use std::sync::Arc;

use crate::ext::hooks::{HookSet, HookSpec};
use crate::ext::rules::action::Action;
use crate::ext::rules::matcher::{Facts, Matcher};
use crate::ext::rules::rule_spec::{PhaseName, RuleMode, RuleSpec};
use crate::ext::{
    AttemptCtx, Extension, Flow, HttpRequestCtx, HttpResponseCtx, RequestCtx, ResponseCtx,
    ResponseFlow,
};

/// One compiled rule.
#[derive(Debug)]
struct Rule {
    name: String,
    phase: PhaseName,
    shadow: bool,
    matcher: Matcher,
    actions: Vec<Action>,
}

impl Rule {
    /// True when the matching rule should act. A shadow rule only logs what it would do.
    fn acts(&self) -> bool {
        if self.shadow {
            let actions: Vec<String> = self.actions.iter().map(Action::describe).collect();
            tracing::info!(
                rule = %self.name,
                phase = self.phase.as_str(),
                actions = %actions.join(", "),
                "A shadow rule matched. The hub took no action."
            );
        }
        !self.shadow
    }
}

/// The rules of one rules file, grouped by phase, in file order.
#[derive(Debug, Default)]
pub(crate) struct RulesExtension {
    hooks: Option<Arc<HookSet>>,
    http_request: Vec<Rule>,
    request: Vec<Rule>,
    attempt: Vec<Rule>,
    response: Vec<Rule>,
    http_response: Vec<Rule>,
}

impl RulesExtension {
    /// Compiles `specs`. The error names the rule and what is wrong with it.
    pub fn compile(
        specs: Vec<RuleSpec>,
        hooks: Vec<HookSpec>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<RulesExtension, String> {
        let mut rules = RulesExtension {
            hooks: HookSet::compile(hooks, env)?.map(Arc::new),
            ..RulesExtension::default()
        };
        let mut names = std::collections::HashSet::new();
        for spec in specs {
            let name = spec.name.trim().to_owned();
            if name.is_empty() {
                return Err("a rule has an empty name".to_owned());
            }
            if !names.insert(name.clone()) {
                return Err(format!("rule `{name}` is defined twice"));
            }
            if spec.actions.is_empty() {
                return Err(format!("rule `{name}`: it has no actions"));
            }
            let phase = spec.phase;
            if phase == PhaseName::Complete {
                return Err(format!(
                    "rule `{name}`: the complete phase runs hooks only, because a rule has nothing to change there"
                ));
            }
            let rule = Rule {
                name: name.clone(),
                phase,
                shadow: spec.mode == RuleMode::Shadow,
                matcher: Matcher::compile(spec.matches, phase)
                    .map_err(|e| format!("rule `{name}`: {e}"))?,
                actions: spec
                    .actions
                    .into_iter()
                    .map(|action| Action::compile(action, phase, env))
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("rule `{name}`: {e}"))?,
            };
            match phase {
                PhaseName::HttpRequest => rules.http_request.push(rule),
                PhaseName::Request => rules.request.push(rule),
                PhaseName::Attempt => rules.attempt.push(rule),
                PhaseName::Response => rules.response.push(rule),
                PhaseName::HttpResponse => rules.http_response.push(rule),
                PhaseName::Complete => {}
            }
        }
        Ok(rules)
    }

    /// The number of rules in every phase.
    pub fn len(&self) -> usize {
        self.all().count()
    }

    /// The number of rules that only log what they would do.
    pub fn shadow_count(&self) -> usize {
        self.all().filter(|rule| rule.shadow).count()
    }

    /// The number of HTTP hooks, and how many of them only log what they would do.
    pub fn hook_counts(&self) -> (usize, usize) {
        self.hooks
            .as_ref()
            .map_or((0, 0), |hooks| (hooks.len(), hooks.shadow_count()))
    }

    /// Hands the hooks to the extension set, which calls them in the async phases.
    pub fn take_hooks(&mut self) -> Option<Arc<HookSet>> {
        self.hooks.take()
    }

    fn all(&self) -> impl Iterator<Item = &Rule> {
        self.http_request
            .iter()
            .chain(&self.request)
            .chain(&self.attempt)
            .chain(&self.response)
            .chain(&self.http_response)
    }

    fn response_facts<'c>(
        ctx: &'c ResponseCtx<'_>,
        error_code: Option<&'c str>,
        error_message: Option<&'c str>,
    ) -> Facts<'c> {
        Facts {
            path: Some(ctx.client_path()),
            api: Some(ctx.api()),
            model: Some(ctx.model()),
            profile: Some(ctx.profile()),
            headers: Some(ctx.headers()),
            status: Some(ctx.status().as_u16()),
            error_code,
            error_message,
            ..Facts::default()
        }
    }
}

impl Extension for RulesExtension {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn on_http_request(&self, ctx: &mut HttpRequestCtx<'_>) -> Flow {
        for rule in &self.http_request {
            let facts = Facts {
                path: Some(ctx.path()),
                method: Some(ctx.method()),
                api: Some(ctx.api()),
                headers: Some(ctx.headers()),
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) || !rule.acts() {
                continue;
            }
            for action in &rule.actions {
                if let Some(rejection) = action.apply_to(ctx) {
                    return rejection;
                }
            }
        }
        Flow::Continue
    }

    fn on_http_response(&self, ctx: &mut HttpResponseCtx<'_>) {
        for rule in &self.http_response {
            let facts = Facts {
                path: Some(ctx.path()),
                method: Some(ctx.method()),
                api: Some(ctx.api()),
                headers: Some(ctx.headers()),
                status: Some(ctx.status().as_u16()),
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) || !rule.acts() {
                continue;
            }
            for action in &rule.actions {
                if let Action::SetStatus(status) = action {
                    ctx.set_status(*status);
                } else {
                    action.apply_to(ctx);
                }
            }
        }
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        for rule in &self.request {
            let (model, stream) = if rule.matcher.needs_request_body() {
                (ctx.model().map(str::to_owned), Some(ctx.stream()))
            } else {
                (None, None)
            };
            let facts = Facts {
                path: Some(ctx.path()),
                method: Some(ctx.method()),
                api: Some(ctx.api()),
                model: model.as_deref(),
                headers: Some(ctx.headers()),
                stream,
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) || !rule.acts() {
                continue;
            }
            for action in &rule.actions {
                if let Some(rejection) = action.apply_to(ctx) {
                    return rejection;
                }
            }
        }
        Flow::Continue
    }

    fn on_attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
        for rule in &self.attempt {
            let stream = rule.matcher.needs_request_body().then(|| ctx.stream());
            let facts = Facts {
                path: Some(ctx.client_path()),
                api: Some(ctx.api()),
                model: Some(ctx.model()),
                profile: Some(ctx.profile()),
                headers: Some(ctx.headers()),
                stream,
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) || !rule.acts() {
                continue;
            }
            for action in &rule.actions {
                if let Some(rejection) = action.apply_to(ctx) {
                    return rejection;
                }
            }
        }
        Flow::Continue
    }

    fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        let facts = Self::response_facts(ctx, None, None);
        self.response.iter().any(|rule| {
            (rule.matcher.needs_error_body()
                || (!rule.shadow && rule.actions.iter().any(Action::changes_body)))
                && rule.matcher.matches(&facts, true)
        })
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        let mut flow = ResponseFlow::Continue;
        for rule in &self.response {
            let error_code = ctx.error_code().map(str::to_owned);
            let error_message = ctx.error_message().map(str::to_owned);
            let facts = Self::response_facts(ctx, error_code.as_deref(), error_message.as_deref());
            if !rule.matcher.matches(&facts, false) || !rule.acts() {
                continue;
            }
            for action in &rule.actions {
                match action {
                    Action::RetryNext => flow = ResponseFlow::RetryNext,
                    Action::SetStatus(status) => ctx.set_status(*status),
                    _ => {
                        action.apply_to(ctx);
                    }
                }
            }
        }
        flow
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode};
    use bytes::Bytes;
    use serde_json::{Value, json};

    use super::*;
    use crate::ext::rules::parse;
    use crate::ext::{Api, AttemptTarget, ExtNotification, ExtSeverity, ResponseTarget};
    use crate::utils::log_capture::LogCapture;

    fn rules(yaml: &str) -> RulesExtension {
        let env = |name: &str| (name == "TEAM").then(|| "blue".to_owned());
        parse(yaml, false, &env).unwrap()
    }

    fn request(
        rules: &RulesExtension,
        headers: &HeaderMap,
        body: &str,
    ) -> (Flow, Option<HeaderMap>, Bytes) {
        let mut state = Extensions::new();
        let mut ctx = RequestCtx::new(
            &Method::POST,
            "/v1/chat/completions",
            headers,
            Bytes::from(body.to_owned()),
            &mut state,
        );
        let flow = rules.on_request(&mut ctx);
        let (headers, body) = ctx.into_parts();
        (flow, headers, body)
    }

    fn attempt(
        rules: &RulesExtension,
        profile: &str,
        body: &str,
    ) -> (Flow, Option<HeaderMap>, Bytes) {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let model = format!("{profile}/m");
        let target = AttemptTarget {
            profile,
            model: &model,
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        };
        let mut ctx = AttemptCtx::new(target, &headers, Bytes::from(body.to_owned()), &mut state);
        let flow = rules.on_attempt(&mut ctx);
        let (headers, body) = ctx.into_parts();
        (flow, headers, body)
    }

    struct Reply {
        wants: bool,
        flow: ResponseFlow,
        status: StatusCode,
        body: Option<Value>,
        headers: HeaderMap,
    }

    fn response(rules: &RulesExtension, status: u16, body: Value) -> Reply {
        let mut headers = HeaderMap::new();
        headers.insert("x-upstream", HeaderValue::from_static("1"));
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "copilot",
            model: "copilot/gpt-5",
            api: Api::Responses,
            client_path: "/v1/responses",
            copilot: true,
            last_attempt: false,
        };
        let mut ctx = ResponseCtx::new(
            target,
            StatusCode::from_u16(status).unwrap(),
            &mut headers,
            &mut state,
        );
        let wants = rules.wants_error_body(&ctx);
        if wants {
            ctx.set_error_body(Some(body));
        }
        let flow = rules.on_response(&mut ctx);
        let (status, body) = ctx.into_parts();
        Reply {
            wants,
            flow,
            status,
            body,
            headers,
        }
    }

    #[test]
    fn a_request_rule_rejects_a_matching_client() {
        let rules = rules(
            "rules:\n  - name: no-opus-for-ci\n    phase: request\n    match: {header: {x-client: ci}, model: '*/claude-opus-*'}\n    actions:\n      - reject: {status: 403, message: CI may not use Opus}\n",
        );
        let mut ci = HeaderMap::new();
        ci.insert("x-client", HeaderValue::from_static("ci"));

        let (flow, _, _) = request(&rules, &ci, r#"{"model":"copilot/claude-opus-4-5"}"#);
        assert_eq!(flow, Flow::reject(403, "CI may not use Opus"));

        let (flow, headers, _) = request(&rules, &ci, r#"{"model":"copilot/gpt-5"}"#);
        assert_eq!(flow, Flow::Continue);
        assert!(headers.is_none());
        let (flow, _, _) = request(
            &rules,
            &HeaderMap::new(),
            r#"{"model":"copilot/claude-opus-4-5"}"#,
        );
        assert_eq!(flow, Flow::Continue);
    }

    #[test]
    fn request_rules_change_headers_and_body_and_set_fallbacks() {
        let rules = rules(
            "rules:\n  - name: chain\n    phase: request\n    match: {stream: true, header_absent: x-llm-hub-fallbacks}\n    actions:\n      - set_fallbacks: [a/b, c/d]\n      - set_field: {/metadata/team: '${TEAM}'}\n      - remove_field: /temperature\n",
        );

        let (flow, headers, body) = request(
            &rules,
            &HeaderMap::new(),
            r#"{"model":"x/y","stream":true,"temperature":1}"#,
        );

        assert_eq!(flow, Flow::Continue);
        assert_eq!(headers.unwrap()["x-llm-hub-fallbacks"], "a/b,c/d");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"model": "x/y", "stream": true, "metadata": {"team": "blue"}})
        );
    }

    #[test]
    fn a_shadow_rule_logs_what_it_would_do_and_changes_nothing() {
        let logs = LogCapture::default();
        let _guard = logs.install();
        let rules = rules(
            "rules:\n  - name: trial-gate\n    phase: request\n    mode: shadow\n    match: {model: 'a/*'}\n    actions:\n      - set_header: {x-seen: '1'}\n      - reject: {status: 403, message: closed}\n  - name: trial-attempt\n    phase: attempt\n    mode: shadow\n    actions:\n      - remove_field: /temperature\n  - name: trial-reply\n    phase: response\n    mode: shadow\n    match: {status: 400}\n    actions:\n      - set_status: 429\n      - retry_next: true\n",
        );
        assert_eq!((rules.len(), rules.shadow_count()), (3, 3));

        let (flow, headers, body) = request(
            &rules,
            &HeaderMap::new(),
            r#"{"model":"a/m","temperature":1}"#,
        );
        let (attempt_flow, _, attempt_body) = attempt(&rules, "p", r#"{"temperature":1}"#);
        let reply = response(&rules, 400, json!({"error": {"code": "x"}}));

        assert_eq!(flow, Flow::Continue);
        assert!(headers.is_none());
        assert_eq!(&body[..], br#"{"model":"a/m","temperature":1}"#);
        assert_eq!(attempt_flow, Flow::Continue);
        assert_eq!(&attempt_body[..], br#"{"temperature":1}"#);
        assert!(!reply.wants);
        assert_eq!(reply.flow, ResponseFlow::Continue);
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let written = logs.text();
        assert!(written.contains("trial-gate"), "{written}");
        assert!(written.contains("set_header, reject(403)"), "{written}");
        assert!(written.contains("phase=\"request\""), "{written}");
        assert!(
            written.contains("trial-attempt") && written.contains("remove_field"),
            "{written}"
        );
        assert!(written.contains("set_status(429), retry_next"), "{written}");
    }

    #[test]
    fn a_rule_without_a_mode_acts() {
        let rules = rules(
            "rules:\n  - name: gate\n    phase: request\n    mode: enforce\n    actions:\n      - reject: {status: 403, message: closed}\n",
        );

        let (flow, _, _) = request(&rules, &HeaderMap::new(), r#"{"model":"a/m"}"#);

        assert_eq!(flow, Flow::reject(403, "closed"));
        assert_eq!(rules.shadow_count(), 0);
    }

    #[test]
    fn a_later_request_rule_matches_the_model_an_earlier_rule_set() {
        let rules = rules(
            "rules:\n  - name: swap\n    phase: request\n    match: {model: 'a/*'}\n    actions:\n      - set_model: b/m\n  - name: gate\n    phase: request\n    match: {model: 'b/*'}\n    actions:\n      - reject: {status: 403, message: b is closed}\n",
        );

        let (flow, _, _) = request(&rules, &HeaderMap::new(), r#"{"model":"a/m"}"#);
        assert_eq!(flow, Flow::reject(403, "b is closed"));

        let (flow, _, body) = request(&rules, &HeaderMap::new(), r#"{"model":"c/m"}"#);
        assert_eq!(flow, Flow::Continue);
        assert_eq!(&body[..], br#"{"model":"c/m"}"#);
    }

    #[test]
    fn a_later_rule_matches_the_stream_flag_an_earlier_rule_set() {
        let rules = rules(
            "rules:\n  - name: force\n    phase: request\n    actions:\n      - set_field: {/stream: true}\n  - name: gate\n    phase: request\n    match: {stream: true}\n    actions:\n      - reject: {status: 400, message: no streams}\n",
        );

        let (flow, _, _) = request(
            &rules,
            &HeaderMap::new(),
            r#"{"model":"a/m","stream":false}"#,
        );

        assert_eq!(flow, Flow::reject(400, "no streams"));
    }

    #[test]
    fn attempt_rules_apply_per_profile() {
        let rules = rules(
            "rules:\n  - name: groq-only\n    phase: attempt\n    match: {profile: groq, model: 'groq/*', path: /v1/chat/*, api: chat, stream: false}\n    actions:\n      - set_header: {x-team: '${TEAM}'}\n      - set_model: llama-fast\n  - name: block-local\n    phase: attempt\n    match: {profile: local}\n    actions:\n      - reject: {status: 503, message: local is off}\n",
        );

        let (flow, headers, body) = attempt(&rules, "groq", r#"{"model":"m"}"#);
        assert_eq!(flow, Flow::Continue);
        assert_eq!(headers.unwrap()["x-team"], "blue");
        assert_eq!(&body[..], br#"{"model":"llama-fast"}"#);

        let (flow, headers, body) = attempt(&rules, "openai", r#"{"model":"m"}"#);
        assert_eq!(flow, Flow::Continue);
        assert!(headers.is_none());
        assert_eq!(&body[..], br#"{"model":"m"}"#);

        let (flow, _, _) = attempt(&rules, "local", r#"{"model":"m"}"#);
        assert_eq!(flow, Flow::reject(503, "local is off"));
    }

    #[test]
    fn response_rules_read_the_error_body_only_when_they_need_it() {
        let rules = rules(
            "rules:\n  - name: rate\n    phase: response\n    match: {profile: copilot, status: 4xx, error_code: rate_limited}\n    actions:\n      - retry_next: true\n      - set_status: 429\n      - set_header: {retry-after: '5'}\n      - remove_header: x-upstream\n      - set_field: {/error/type: rate_limit_error}\n",
        );

        let reply = response(&rules, 400, json!({"error": {"code": "rate_limited"}}));
        assert!(reply.wants);
        assert_eq!(reply.flow, ResponseFlow::RetryNext);
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            reply.body,
            Some(json!({"error": {"code": "rate_limited", "type": "rate_limit_error"}}))
        );
        assert_eq!(reply.headers["retry-after"], "5");
        assert!(!reply.headers.contains_key("x-upstream"));

        let other = response(&rules, 400, json!({"error": {"code": "other"}}));
        assert!(other.wants);
        assert_eq!(other.flow, ResponseFlow::Continue);
        assert_eq!(other.status, StatusCode::BAD_REQUEST);

        let ok = response(&rules, 200, json!({}));
        assert!(!ok.wants);
        assert_eq!(ok.flow, ResponseFlow::Continue);
    }

    #[test]
    fn a_response_rule_can_match_the_error_message() {
        let rules = rules(
            "rules:\n  - name: warming\n    phase: response\n    match: {status: 400, error_message: 'model is warming up'}\n    actions:\n      - retry_next: true\n",
        );

        let warming = response(
            &rules,
            400,
            json!({"error": {"message": "The model is WARMING UP, retry"}}),
        );
        assert!(warming.wants);
        assert_eq!(warming.flow, ResponseFlow::RetryNext);

        let schema = response(&rules, 400, json!({"error": {"message": "bad schema"}}));
        assert_eq!(schema.flow, ResponseFlow::Continue);
    }

    #[test]
    fn a_header_only_response_rule_does_not_read_the_body() {
        let rules = rules(
            "rules:\n  - name: tag\n    phase: response\n    match: {api: responses, model: 'copilot/*', path: /v1/responses, header: {x-upstream: '1'}}\n    actions:\n      - set_header: {x-seen: yes}\n",
        );

        let reply = response(&rules, 500, json!({}));

        assert!(!reply.wants);
        assert_eq!(reply.headers["x-seen"], "yes");
    }

    #[test]
    fn invalid_rule_files_name_the_problem() {
        let env = |_: &str| None;
        for (yaml, expected) in [
            (
                "rules: [{name: '', phase: request, actions: [{reject: {status: 400, message: x}}]}]",
                "empty name",
            ),
            (
                "rules: [{name: a, phase: request, actions: [{reject: {status: 400, message: x}}]}, {name: a, phase: request, actions: [{reject: {status: 400, message: x}}]}]",
                "defined twice",
            ),
            (
                "rules: [{name: a, phase: request, actions: []}]",
                "no actions",
            ),
            (
                "rules: [{name: a, phase: request, match: {profile: x}, actions: [{reject: {status: 400, message: x}}]}]",
                "rule `a`",
            ),
            (
                "rules: [{name: a, phase: request, actions: [{retry_next: true}]}]",
                "rule `a`",
            ),
            (
                "rules: [{name: a, phase: request, typo: 1, actions: []}]",
                "typo",
            ),
        ] {
            let error = parse(yaml, false, &env).unwrap_err();
            assert!(error.contains(expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn http_phase_rules_accept_only_their_keys_and_actions() {
        let env = |_: &str| None;
        for (yaml, expected) in [
            (
                "rules: [{name: a, phase: complete, actions: [{set_header: {x: '1'}}]}]",
                "hooks only",
            ),
            (
                "rules: [{name: a, phase: http_request, match: {model: 'x/*'}, actions: [{reject: {status: 400, message: m}}]}]",
                "match key `model`",
            ),
            (
                "rules: [{name: a, phase: http_request, actions: [{set_status: 400}]}]",
                "action `set_status`",
            ),
            (
                "rules: [{name: a, phase: http_response, actions: [{reject: {status: 400, message: m}}]}]",
                "action `reject`",
            ),
            (
                "rules: [{name: a, phase: http_response, match: {stream: true}, actions: [{set_status: 400}]}]",
                "match key `stream`",
            ),
        ] {
            let error = parse(yaml, false, &env).unwrap_err();
            assert!(error.contains(expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn notify_rules_send_from_every_phase_with_the_known_model_and_profile() {
        let rules = rules(
            "rules:\n  - name: n-http\n    phase: http_request\n    actions: [{notify: {title: 'http ${TEAM}', severity: info}}]\n  - name: n-request\n    phase: request\n    actions: [{notify: {title: request, body: b}}]\n  - name: n-attempt\n    phase: attempt\n    actions: [{notify: {title: attempt, severity: critical}}]\n  - name: n-response\n    phase: response\n    actions: [{notify: {title: response}}]\n  - name: n-out\n    phase: http_response\n    actions: [{notify: {title: out}}]\n  - name: n-shadow\n    phase: request\n    mode: shadow\n    actions: [{notify: {title: shadow}}]\n",
        );
        let mut headers = HeaderMap::new();
        let mut http = HttpRequestCtx::new(&Method::GET, "/v1/models", &mut headers);
        rules.on_http_request(&mut http);
        let sent = http.outbox().take();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0],
            ExtNotification::new(ExtSeverity::Info, "http blue", "")
        );
        let mut out =
            HttpResponseCtx::new(&Method::GET, "/v1/models", StatusCode::OK, &mut headers);
        rules.on_http_response(&mut out);
        assert_eq!(out.outbox().take()[0].title, "out");

        let mut state = Extensions::new();
        let empty = HeaderMap::new();
        let mut request = RequestCtx::new(
            &Method::POST,
            "/v1/chat/completions",
            &empty,
            Bytes::from_static(br#"{"model":"a/b"}"#),
            &mut state,
        );
        rules.on_request(&mut request);
        assert_eq!(
            request.outbox().take(),
            [ExtNotification::new(ExtSeverity::Warning, "request", "b").with_model("a/b")]
        );

        let target = AttemptTarget {
            profile: "p",
            model: "p/m",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        };
        let mut attempt = AttemptCtx::new(target, &empty, Bytes::new(), &mut state);
        rules.on_attempt(&mut attempt);
        assert_eq!(
            attempt.outbox().take(),
            [ExtNotification::new(ExtSeverity::Critical, "attempt", "")
                .with_model("p/m")
                .with_profile("p")]
        );

        let target = ResponseTarget {
            profile: "copilot",
            model: "copilot/gpt-5",
            api: Api::Responses,
            client_path: "/v1/responses",
            copilot: true,
            last_attempt: false,
        };
        let mut reply_headers = HeaderMap::new();
        let mut reply = ResponseCtx::new(target, StatusCode::OK, &mut reply_headers, &mut state);
        assert!(!rules.wants_error_body(&reply));
        rules.on_response(&mut reply);
        let sent = reply.outbox().take();
        assert_eq!(sent[0].model.as_deref(), Some("copilot/gpt-5"));
        assert_eq!(sent[0].profile.as_deref(), Some("copilot"));
    }

    #[test]
    fn a_notify_action_needs_a_title_and_a_known_severity() {
        let env = |_: &str| None;
        for (yaml, expected) in [
            (
                "rules: [{name: a, phase: request, actions: [{notify: {title: ' '}}]}]",
                "notify needs a title",
            ),
            (
                "rules: [{name: a, phase: request, actions: [{notify: {title: t, severity: loud}}]}]",
                "loud",
            ),
            (
                "rules: [{name: a, phase: request, actions: [{notify: {title: t, extra: 1}}]}]",
                "extra",
            ),
        ] {
            let error = parse(yaml, false, &env).unwrap_err();
            assert!(error.contains(expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn json_and_yaml_rule_files_compile_to_the_same_rules() {
        let env = |_: &str| None;
        let json = parse(
            r#"{"rules": [{"name": "a", "phase": "attempt", "actions": [{"remove_header": "authorization"}]}]}"#,
            true,
            &env,
        )
        .unwrap();
        let yaml = parse(
            "rules: [{name: a, phase: attempt, actions: [{remove_header: authorization}]}]",
            false,
            &env,
        )
        .unwrap();

        assert_eq!(json.len(), 1);
        assert_eq!(yaml.len(), 1);
        assert_eq!(parse("{}", true, &env).unwrap().len(), 0);
        assert_eq!(json.name(), "rules");
    }
}

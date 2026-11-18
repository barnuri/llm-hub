use crate::ext::rules::action::{Action, apply_body, apply_headers};
use crate::ext::rules::matcher::{Facts, Matcher};
use crate::ext::rules::rule_spec::{PhaseName, RuleSpec};
use crate::ext::{AttemptCtx, Extension, Flow, RequestCtx, ResponseCtx, ResponseFlow};

/// One compiled rule.
#[derive(Debug)]
struct Rule {
    matcher: Matcher,
    actions: Vec<Action>,
}

/// The rules of one rules file, grouped by phase, in file order.
#[derive(Debug, Default)]
pub(crate) struct RulesExtension {
    request: Vec<Rule>,
    attempt: Vec<Rule>,
    response: Vec<Rule>,
}

impl RulesExtension {
    /// Compiles `specs`. The error names the rule and what is wrong with it.
    pub fn compile(
        specs: Vec<RuleSpec>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<RulesExtension, String> {
        let mut rules = RulesExtension::default();
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
            let rule = Rule {
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
                PhaseName::Request => rules.request.push(rule),
                PhaseName::Attempt => rules.attempt.push(rule),
                PhaseName::Response => rules.response.push(rule),
            }
        }
        Ok(rules)
    }

    pub fn len(&self) -> usize {
        self.request.len() + self.attempt.len() + self.response.len()
    }

    fn response_facts<'c>(ctx: &'c ResponseCtx<'_>, error_code: Option<&'c str>) -> Facts<'c> {
        Facts {
            path: Some(ctx.client_path()),
            api: Some(ctx.api()),
            model: Some(ctx.model()),
            profile: Some(ctx.profile()),
            headers: Some(ctx.headers()),
            status: Some(ctx.status().as_u16()),
            error_code,
            ..Facts::default()
        }
    }
}

impl Extension for RulesExtension {
    fn name(&self) -> &'static str {
        "rules"
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        let needs_body = self
            .request
            .iter()
            .any(|rule| rule.matcher.needs_request_body());
        let model = needs_body.then(|| ctx.model().map(str::to_owned)).flatten();
        let stream = needs_body.then(|| ctx.stream());
        for rule in &self.request {
            let path = ctx.path().to_owned();
            let facts = Facts {
                path: Some(&path),
                method: Some(ctx.method()),
                api: Some(ctx.api()),
                model: model.as_deref(),
                headers: Some(ctx.headers()),
                stream,
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) {
                continue;
            }
            for action in &rule.actions {
                if let Action::Reject(status, message) = action {
                    return Flow::reject(status.as_u16(), message.clone());
                }
                apply_headers(action, ctx.headers_mut());
                if action.changes_body()
                    && let Some(body) = ctx.body_mut()
                {
                    apply_body(action, body);
                }
            }
        }
        Flow::Continue
    }

    fn on_attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
        let needs_stream = self
            .attempt
            .iter()
            .any(|rule| rule.matcher.needs_request_body());
        let stream = needs_stream.then(|| ctx.stream());
        for rule in &self.attempt {
            let facts = Facts {
                path: Some(ctx.client_path()),
                api: Some(ctx.api()),
                model: Some(ctx.model()),
                profile: Some(ctx.profile()),
                headers: Some(ctx.headers()),
                stream,
                ..Facts::default()
            };
            if !rule.matcher.matches(&facts, false) {
                continue;
            }
            for action in &rule.actions {
                if let Action::Reject(status, message) = action {
                    return Flow::reject(status.as_u16(), message.clone());
                }
                apply_headers(action, ctx.headers_mut());
                if action.changes_body()
                    && let Some(body) = ctx.body_mut()
                {
                    apply_body(action, body);
                }
            }
        }
        Flow::Continue
    }

    fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        let facts = Self::response_facts(ctx, None);
        self.response.iter().any(|rule| {
            (rule.matcher.needs_error_body() || rule.actions.iter().any(Action::changes_body))
                && rule.matcher.matches(&facts, true)
        })
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        let mut flow = ResponseFlow::Continue;
        for rule in &self.response {
            let error_code = ctx.error_code().map(str::to_owned);
            if !rule
                .matcher
                .matches(&Self::response_facts(ctx, error_code.as_deref()), false)
            {
                continue;
            }
            for action in &rule.actions {
                match action {
                    Action::RetryNext => flow = ResponseFlow::RetryNext,
                    Action::SetStatus(status) => ctx.set_status(*status),
                    _ => {
                        apply_headers(action, ctx.headers_mut());
                        if action.changes_body()
                            && let Some(body) = ctx.error_body_mut()
                        {
                            apply_body(action, body);
                        }
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
    use crate::ext::{Api, AttemptTarget, ResponseTarget};

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

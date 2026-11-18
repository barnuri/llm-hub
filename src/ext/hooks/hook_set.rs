use std::collections::HashSet;

use axum::http::{HeaderMap, StatusCode, header};
use serde_json::{Map, Value, json};

use crate::ext::hooks::hook::Hook;
use crate::ext::hooks::hook_reply::Effect;
use crate::ext::hooks::hook_spec::HookSpec;
use crate::ext::rules::{ActionTarget, Facts, PhaseName};
use crate::ext::{AttemptCtx, CompleteCtx, Flow, RequestCtx};

/// Headers a hook never sees: they carry the client's or the upstream's credentials.
const WITHHELD_HEADERS: [header::HeaderName; 4] = [
    header::AUTHORIZATION,
    header::PROXY_AUTHORIZATION,
    header::COOKIE,
    header::HeaderName::from_static("x-api-key"),
];

/// The hooks of one rules file, grouped by phase, in file order.
#[derive(Debug)]
pub(crate) struct HookSet {
    client: reqwest::Client,
    request: Vec<Hook>,
    attempt: Vec<Hook>,
    complete: Vec<Hook>,
}

impl HookSet {
    /// Compiles `specs`. `None` when there are none.
    ///
    /// # Errors
    /// A message that names the hook and what is wrong with it.
    pub(crate) fn compile(
        specs: Vec<HookSpec>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<HookSet>, String> {
        if specs.is_empty() {
            return Ok(None);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("the hook client could not start: {e}"))?;
        let mut set = HookSet {
            client,
            request: Vec::new(),
            attempt: Vec::new(),
            complete: Vec::new(),
        };
        let mut names = HashSet::new();
        for spec in specs {
            let hook = Hook::compile(spec, env)?;
            if !names.insert(hook.name.clone()) {
                return Err(format!("hook `{}` is defined twice", hook.name));
            }
            match hook.phase {
                PhaseName::Request => set.request.push(hook),
                PhaseName::Attempt => set.attempt.push(hook),
                _ => set.complete.push(hook),
            }
        }
        Ok(Some(set))
    }

    /// The number of hooks in every phase.
    pub(crate) fn len(&self) -> usize {
        self.all().count()
    }

    /// The number of hooks that only log what they would do.
    pub(crate) fn shadow_count(&self) -> usize {
        self.all().filter(|hook| hook.shadow).count()
    }

    fn all(&self) -> impl Iterator<Item = &Hook> {
        self.request
            .iter()
            .chain(&self.attempt)
            .chain(&self.complete)
    }

    /// Calls the matching `request` hooks in file order. Each sees the changes of the one before.
    pub(crate) async fn request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        for hook in &self.request {
            let body_needed = hook.send_body || hook.matcher.needs_request_body();
            let (model, stream) = if body_needed {
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
            if !hook.matcher.matches(&facts, false) {
                continue;
            }
            let mut payload = json!({
                "phase": "request",
                "method": ctx.method().as_str(),
                "path": ctx.path(),
                "api": ctx.api().as_str(),
                "model": model,
                "headers": sendable_headers(ctx.headers()),
            });
            if hook.send_body {
                payload["body"] = ctx.body().cloned().unwrap_or(Value::Null);
            }
            if let flow @ Flow::Reject { .. } = self.run(hook, &payload, ctx).await {
                return flow;
            }
        }
        Flow::Continue
    }

    /// Calls the matching `attempt` hooks in file order.
    pub(crate) async fn attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
        for hook in &self.attempt {
            let stream = hook.matcher.needs_request_body().then(|| ctx.stream());
            let facts = Facts {
                path: Some(ctx.client_path()),
                api: Some(ctx.api()),
                model: Some(ctx.model()),
                profile: Some(ctx.profile()),
                headers: Some(ctx.headers()),
                stream,
                ..Facts::default()
            };
            if !hook.matcher.matches(&facts, false) {
                continue;
            }
            let mut payload = json!({
                "phase": "attempt",
                "path": ctx.client_path(),
                "upstream_path": ctx.upstream_path(),
                "api": ctx.api().as_str(),
                "profile": ctx.profile(),
                "model": ctx.model(),
                "headers": sendable_headers(ctx.headers()),
            });
            if hook.send_body {
                payload["body"] = ctx.body().cloned().unwrap_or(Value::Null);
            }
            if let flow @ Flow::Reject { .. } = self.run(hook, &payload, ctx).await {
                return flow;
            }
        }
        Flow::Continue
    }

    /// Posts the outcome to each matching `complete` hook on a spawned task, so the response is
    /// never delayed. A failed call is logged, whatever the hook's `fail` setting.
    pub(crate) fn complete(&self, ctx: &CompleteCtx<'_>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for hook in &self.complete {
            let facts = Facts {
                profile: Some(ctx.profile()),
                model: Some(ctx.model()),
                status: Some(ctx.status()),
                ..Facts::default()
            };
            if !hook.matcher.matches(&facts, false) {
                continue;
            }
            let payload = json!({
                "phase": "complete",
                "profile": ctx.profile(),
                "model": ctx.model(),
                "status": ctx.status(),
                "latency_ms": ctx.latency_ms(),
                "ttft_ms": ctx.ttft_ms(),
                "tokens_in": ctx.tokens_in(),
                "tokens_out": ctx.tokens_out(),
                "error": ctx.error(),
                "client": ctx.client(),
            });
            let (client, url, name, timeout) = (
                self.client.clone(),
                hook.url.clone(),
                hook.name.clone(),
                hook.timeout,
            );
            runtime.spawn(async move {
                let sent = client
                    .post(url)
                    .timeout(timeout)
                    .json(&payload)
                    .send()
                    .await;
                match sent {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => tracing::warn!(
                        hook = %name,
                        "The complete hook answered HTTP {}.",
                        response.status()
                    ),
                    Err(e) => tracing::warn!(hook = %name, "The complete hook call failed: {e}"),
                }
            });
        }
    }

    /// Calls one hook and applies its answer. A shadow hook is called and logged only.
    async fn run(&self, hook: &Hook, payload: &Value, target: &mut impl ActionTarget) -> Flow {
        match hook.invoke(&self.client, payload).await {
            Ok(effect) if hook.shadow => {
                tracing::info!(
                    hook = %hook.name,
                    phase = hook.phase.as_str(),
                    verdict = %effect.describe(),
                    "A shadow hook answered. The hub took no action."
                );
                Flow::Continue
            }
            Ok(Effect::Continue) => Flow::Continue,
            Ok(Effect::Reject(status, message)) => Flow::Reject { status, message },
            Ok(Effect::Apply(actions)) => {
                for action in &actions {
                    action.apply_to(target);
                }
                Flow::Continue
            }
            Err(reason) if hook.fail_closed && !hook.shadow => Flow::Reject {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("The extension hook `{}` failed. {reason}", hook.name),
            },
            Err(reason) => {
                tracing::warn!(hook = %hook.name, "The hook failed and the hub skipped it. {reason}");
                Flow::Continue
            }
        }
    }
}

/// The headers as a JSON object, without the credentials in [`WITHHELD_HEADERS`].
fn sendable_headers(headers: &HeaderMap) -> Value {
    let mut map = Map::new();
    for (name, value) in headers {
        if WITHHELD_HEADERS.contains(name) {
            continue;
        }
        if let Ok(text) = value.to_str() {
            map.insert(name.as_str().to_owned(), Value::from(text));
        }
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Extensions, HeaderValue, Method};
    use bytes::Bytes;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ext::rules::parse;
    use crate::ext::{Api, AttemptTarget};
    use crate::services::stats::RequestOutcome;
    use crate::utils::log_capture::LogCapture;

    fn hooks(yaml: &str, base: &str) -> std::sync::Arc<HookSet> {
        let base = base.to_owned();
        let env = move |name: &str| (name == "HOOK").then(|| base.clone());
        parse(yaml, false, &env)
            .unwrap()
            .take_hooks()
            .expect("the file has hooks")
    }

    async fn answer(server: &MockServer, template: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(template)
            .mount(server)
            .await;
    }

    fn reply(body: &Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(body)
    }

    async fn run_request(
        set: &HookSet,
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
        let flow = set.request(&mut ctx).await;
        let (changed, body) = ctx.into_parts();
        (flow, changed, body)
    }

    async fn run_attempt(set: &HookSet, body: &str) -> (Flow, Option<HeaderMap>, Bytes) {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = AttemptTarget {
            profile: "groq",
            model: "groq/llama",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        };
        let mut ctx = AttemptCtx::new(target, &headers, Bytes::from(body.to_owned()), &mut state);
        let flow = set.attempt(&mut ctx).await;
        let (changed, body) = ctx.into_parts();
        (flow, changed, body)
    }

    async fn sent(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    const REQUEST_HOOK: &str =
        "hooks:\n  - name: guard\n    phase: request\n    url: '${HOOK}/hook'\n";

    #[tokio::test]
    async fn a_request_hook_gets_the_metadata_and_never_the_credentials() {
        let server = MockServer::start().await;
        answer(&server, reply(&json!({"action": "continue"}))).await;
        let set = hooks(REQUEST_HOOK, &server.uri());
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer secret"),
            ("x-api-key", "secret"),
            ("cookie", "a=b"),
            ("proxy-authorization", "x"),
            ("x-team", "blue"),
        ] {
            headers.insert(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_static(value),
            );
        }

        let (flow, changed, _) =
            run_request(&set, &headers, r#"{"model":"a/m","stream":true}"#).await;

        assert_eq!(flow, Flow::Continue);
        assert!(changed.is_none());
        let payloads = sent(&server).await;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["phase"], "request");
        assert_eq!(payloads[0]["method"], "POST");
        assert_eq!(payloads[0]["path"], "/v1/chat/completions");
        assert_eq!(payloads[0]["api"], "chat");
        assert_eq!(payloads[0]["headers"], json!({"x-team": "blue"}));
        assert!(payloads[0].get("body").is_none());
        assert!(!payloads[0].to_string().contains("secret"));
    }

    #[tokio::test]
    async fn the_body_and_model_are_sent_only_when_asked() {
        let server = MockServer::start().await;
        answer(&server, reply(&json!({"action": "continue"}))).await;
        let set = hooks(
            "hooks:\n  - name: guard\n    phase: request\n    url: '${HOOK}/hook'\n    send: [headers, body]\n",
            &server.uri(),
        );

        run_request(&set, &HeaderMap::new(), r#"{"model":"a/m","messages":[]}"#).await;

        let payloads = sent(&server).await;
        assert_eq!(payloads[0]["model"], "a/m");
        assert_eq!(payloads[0]["body"], json!({"model": "a/m", "messages": []}));
    }

    #[tokio::test]
    async fn a_patch_changes_headers_and_body_and_the_next_hook_sees_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/first"))
            .respond_with(reply(
                &json!({"action": "patch", "set_headers": {"x-reviewed": "1"},
                "remove_headers": ["x-old"],
                "body_patch": [{"op": "add", "path": "/metadata/team", "value": "blue"},
                               {"op": "remove", "path": "/temperature"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/second"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let set = hooks(
            "hooks:\n  - name: first\n    phase: request\n    url: '${HOOK}/first'\n  - name: second\n    phase: request\n    url: '${HOOK}/second'\n    send: [headers, body]\n",
            &server.uri(),
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-old", HeaderValue::from_static("1"));

        let (flow, changed, body) =
            run_request(&set, &headers, r#"{"model":"a/m","temperature":1}"#).await;

        assert_eq!(flow, Flow::Continue);
        let changed = changed.unwrap();
        assert_eq!(changed["x-reviewed"], "1");
        assert!(!changed.contains_key("x-old"));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"model": "a/m", "metadata": {"team": "blue"}})
        );
        let second = &server.received_requests().await.unwrap()[1];
        let second: Value = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(second["headers"]["x-reviewed"], "1");
        assert_eq!(second["body"]["metadata"]["team"], "blue");
    }

    #[tokio::test]
    async fn a_reject_reply_stops_the_phase() {
        let server = MockServer::start().await;
        answer(
            &server,
            reply(&json!({"action": "reject", "status": 400, "message": "prompt has a secret"})),
        )
        .await;
        let set = hooks(
            &format!(
                "{REQUEST_HOOK}  - name: never\n    phase: request\n    url: '${{HOOK}}/never'\n"
            ),
            &server.uri(),
        );

        let (flow, _, _) = run_request(&set, &HeaderMap::new(), r#"{"model":"a/m"}"#).await;

        assert_eq!(flow, Flow::reject(400, "prompt has a secret"));
        assert_eq!(sent(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn a_hook_that_does_not_match_is_not_called() {
        let server = MockServer::start().await;
        answer(
            &server,
            reply(&json!({"action": "reject", "status": 403, "message": "no"})),
        )
        .await;
        let set = hooks(
            "hooks:\n  - name: opus-only\n    phase: request\n    url: '${HOOK}/hook'\n    match: {model: '*/claude-opus-*', header: {x-client: ci}}\n",
            &server.uri(),
        );

        let (flow, _, _) = run_request(&set, &HeaderMap::new(), r#"{"model":"a/gpt-5"}"#).await;

        assert_eq!(flow, Flow::Continue);
        assert!(sent(&server).await.is_empty());
    }

    async fn failure_cases(server: &MockServer) -> Vec<(&'static str, String)> {
        Mock::given(method("POST"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(500))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/field"))
            .respond_with(reply(&json!({"action": "continue", "extra": true})))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/slow"))
            .respond_with(
                reply(&json!({"action": "continue"})).set_delay(Duration::from_millis(600)),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redirect"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/hook"))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/huge"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b' '; 1024 * 1024 + 1]))
            .mount(server)
            .await;
        let base = server.uri();
        vec![
            ("http status", format!("{base}/status")),
            ("invalid json", format!("{base}/json")),
            ("unknown field", format!("{base}/field")),
            ("timeout", format!("{base}/slow")),
            ("redirect", format!("{base}/redirect")),
            ("huge reply", format!("{base}/huge")),
            ("refused", "http://127.0.0.1:9/hook".to_owned()),
        ]
    }

    #[tokio::test]
    async fn every_kind_of_failure_is_skipped_when_the_hook_fails_open() {
        let server = MockServer::start().await;
        for (case, url) in failure_cases(&server).await {
            let set = hooks(
                &format!(
                    "hooks:\n  - name: guard\n    phase: request\n    url: '{url}'\n    timeout_ms: 200\n"
                ),
                "",
            );

            let (flow, changed, _) =
                run_request(&set, &HeaderMap::new(), r#"{"model":"a/m"}"#).await;

            assert_eq!(flow, Flow::Continue, "{case}");
            assert!(changed.is_none(), "{case}");
        }
    }

    #[tokio::test]
    async fn every_kind_of_failure_rejects_when_the_hook_fails_closed() {
        let server = MockServer::start().await;
        for (case, url) in failure_cases(&server).await {
            let set = hooks(
                &format!(
                    "hooks:\n  - name: guard\n    phase: request\n    url: '{url}'\n    timeout_ms: 200\n    fail: closed\n"
                ),
                "",
            );

            let (flow, _, _) = run_request(&set, &HeaderMap::new(), r#"{"model":"a/m"}"#).await;

            let Flow::Reject { status, message } = flow else {
                panic!("{case}: expected a rejection");
            };
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{case}");
            assert!(message.contains("`guard` failed"), "{case}: {message}");
        }
    }

    #[tokio::test]
    async fn a_shadow_hook_is_called_and_logged_and_changes_nothing() {
        let logs = LogCapture::default();
        let _guard = logs.install();
        let server = MockServer::start().await;
        answer(
            &server,
            reply(&json!({"action": "reject", "status": 403, "message": "no"})),
        )
        .await;
        let set = hooks(
            "hooks:\n  - name: trial\n    phase: request\n    mode: shadow\n    fail: closed\n    url: '${HOOK}/hook'\n  - name: broken\n    phase: request\n    mode: shadow\n    fail: closed\n    url: 'http://127.0.0.1:9/hook'\n    timeout_ms: 100\n",
            &server.uri(),
        );
        assert_eq!((set.len(), set.shadow_count()), (2, 2));

        let (flow, changed, _) = run_request(&set, &HeaderMap::new(), r#"{"model":"a/m"}"#).await;

        assert_eq!(flow, Flow::Continue);
        assert!(changed.is_none());
        assert_eq!(sent(&server).await.len(), 1);
        let written = logs.text();
        assert!(
            written.contains("trial") && written.contains("reject(403)"),
            "{written}"
        );
    }

    #[tokio::test]
    async fn an_attempt_hook_sees_the_target_and_patches_the_attempt_body() {
        let server = MockServer::start().await;
        answer(
            &server,
            reply(&json!({"action": "patch", "body_patch": [{"op": "replace", "path": "/temperature", "value": 0}]})),
        )
        .await;
        let set = hooks(
            "hooks:\n  - name: tune\n    phase: attempt\n    url: '${HOOK}/hook'\n    match: {profile: groq}\n    send: [body]\n  - name: skipped\n    phase: attempt\n    url: '${HOOK}/never'\n    match: {profile: openai}\n",
            &server.uri(),
        );

        let (flow, _, body) = run_attempt(&set, r#"{"model":"llama","temperature":1}"#).await;

        assert_eq!(flow, Flow::Continue);
        assert_eq!(&body[..], br#"{"model":"llama","temperature":0}"#);
        let payload = &sent(&server).await[0];
        assert_eq!(payload["phase"], "attempt");
        assert_eq!(payload["profile"], "groq");
        assert_eq!(payload["model"], "groq/llama");
        assert_eq!(payload["upstream_path"], "/chat/completions");
        assert_eq!(payload["body"]["temperature"], 1);
    }

    #[tokio::test]
    async fn a_complete_hook_gets_the_outcome_off_the_request_path() {
        let server = MockServer::start().await;
        answer(&server, ResponseTemplate::new(200)).await;
        Mock::given(method("POST"))
            .and(path("/errors"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let set = hooks(
            "hooks:\n  - name: audit\n    phase: complete\n    url: '${HOOK}/hook'\n  - name: failures\n    phase: complete\n    match: {status: 429}\n    url: '${HOOK}/errors'\n  - name: unreachable\n    phase: complete\n    url: 'http://127.0.0.1:9/hook'\n    timeout_ms: 100\n",
            &server.uri(),
        );
        let outcome = RequestOutcome {
            profile: "groq".to_owned(),
            model_key: "groq/llama".to_owned(),
            status: 200,
            latency_ms: 12,
            tokens_in: 5,
            tokens_out: 7,
            client: Some("codex".to_owned()),
            ..RequestOutcome::default()
        };

        set.complete(&CompleteCtx::new(&outcome, None));
        for _ in 0..100 {
            if !server.received_requests().await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let failing = RequestOutcome {
            status: 429,
            ..outcome
        };
        set.complete(&CompleteCtx::new(&failing, None));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let payloads = sent(&server).await;
        assert_eq!(payloads.len(), 3);
        let first = payloads.iter().find(|p| p["status"] == 200).unwrap();
        assert_eq!(first["phase"], "complete");
        assert_eq!(first["model"], "groq/llama");
        assert_eq!(first["tokens_out"], 7);
        assert_eq!(first["client"], "codex");
    }

    #[test]
    fn complete_hooks_without_a_runtime_do_nothing() {
        let set = hooks(
            "hooks:\n  - name: audit\n    phase: complete\n    url: 'http://127.0.0.1:9/hook'\n",
            "",
        );

        set.complete(&CompleteCtx::new(&RequestOutcome::default(), None));
    }

    #[test]
    fn invalid_hook_files_name_the_hook_and_the_problem() {
        let env = |name: &str| (name != "MISSING_VAR_X").then(|| "http://127.0.0.1:1".to_owned());
        for (yaml, expected) in [
            (
                "hooks: [{name: '', phase: request, url: 'http://127.0.0.1/x'}]",
                "empty name",
            ),
            (
                "hooks: [{name: a, phase: response, url: 'http://127.0.0.1/x'}]",
                "does not run hooks",
            ),
            (
                "hooks: [{name: a, phase: http_request, url: 'http://127.0.0.1/x'}]",
                "does not run hooks",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://example.com/x'}]",
                "plain http to another machine",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'ftp://127.0.0.1/x'}]",
                "not an http or https URL",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'not a url'}]",
                "is not a URL",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://127.0.0.1/x', timeout_ms: 0}]",
                "timeout_ms 0",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://127.0.0.1/x', timeout_ms: 9000}]",
                "timeout_ms 9000",
            ),
            (
                "hooks: [{name: a, phase: complete, url: 'http://127.0.0.1/x', send: [body]}]",
                "takes no `send`",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://127.0.0.1/x', match: {model: 'a/*', status: 400}}]",
                "match key `status`",
            ),
            (
                "hooks: [{name: a, phase: request, url: '${MISSING_VAR_X}/x'}]",
                "MISSING_VAR_X is not set",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://127.0.0.1/x'}, {name: a, phase: attempt, url: 'http://127.0.0.1/y'}]",
                "defined twice",
            ),
            (
                "hooks: [{name: a, phase: request, url: 'http://127.0.0.1/x', typo: 1}]",
                "typo",
            ),
        ] {
            let error = parse(yaml, false, &env).map(|_| ()).unwrap_err();
            assert!(error.contains(expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn loopback_http_and_any_https_urls_are_accepted() {
        let env = |_: &str| None;
        let yaml = "hooks:\n  - {name: a, phase: request, url: 'http://localhost:9/x'}\n  - {name: b, phase: request, url: 'http://[::1]:9/x'}\n  - {name: c, phase: attempt, url: 'https://hooks.example.com/x'}\n  - {name: d, phase: complete, url: 'http://127.0.0.5:9/x'}\n";

        let mut rules = parse(yaml, false, &env).unwrap();

        assert_eq!(rules.hook_counts(), (4, 0));
        assert_eq!(rules.take_hooks().unwrap().len(), 4);
        assert_eq!(rules.hook_counts(), (0, 0));
    }

    #[test]
    fn a_file_without_hooks_has_none() {
        let env = |_: &str| None;

        let mut rules = parse("rules: []", false, &env).unwrap();

        assert!(rules.take_hooks().is_none());
    }
}

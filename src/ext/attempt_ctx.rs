use std::borrow::Cow;

use axum::http::{Extensions, HeaderMap};
use bytes::Bytes;
use serde_json::Value;

use crate::ext::{Api, ExtNotification, JsonBody, Outbox};

/// The `attempt` phase view of one upstream call: the primary model or one fallback.
#[derive(Debug)]
pub struct AttemptCtx<'a> {
    profile: &'a str,
    model: &'a str,
    api: Api,
    client_path: &'a str,
    upstream_path: &'a str,
    headers: Cow<'a, HeaderMap>,
    body: JsonBody,
    state: &'a mut Extensions,
    outbox: Outbox,
}

/// Where an attempt goes, for [`AttemptCtx::new`].
#[derive(Clone, Copy)]
pub(crate) struct AttemptTarget<'a> {
    pub profile: &'a str,
    pub model: &'a str,
    pub api: Api,
    pub client_path: &'a str,
    pub upstream_path: &'a str,
}

impl<'a> AttemptCtx<'a> {
    pub(crate) fn new(
        target: AttemptTarget<'a>,
        headers: &'a HeaderMap,
        body: Bytes,
        state: &'a mut Extensions,
    ) -> AttemptCtx<'a> {
        AttemptCtx {
            profile: target.profile,
            model: target.model,
            api: target.api,
            client_path: target.client_path,
            upstream_path: target.upstream_path,
            headers: Cow::Borrowed(headers),
            body: JsonBody::new(body),
            state,
            outbox: Outbox::default(),
        }
    }

    /// The headers and body to send. Headers are `None` when no extension changed them.
    pub(crate) fn into_parts(self) -> (Option<HeaderMap>, Bytes) {
        let headers = match self.headers {
            Cow::Owned(changed) => Some(changed),
            Cow::Borrowed(_) => None,
        };
        (headers, self.body.into_bytes())
    }

    /// The profile this attempt goes to, such as `copilot`.
    #[must_use]
    pub fn profile(&self) -> &str {
        self.profile
    }

    /// The model this attempt serves, as `<profile>/<model>`.
    #[must_use]
    pub fn model(&self) -> &str {
        self.model
    }

    /// The client API of the request.
    #[must_use]
    pub fn api(&self) -> Api {
        self.api
    }

    /// The client path, with any query string.
    #[must_use]
    pub fn client_path(&self) -> &str {
        self.client_path
    }

    /// The path sent upstream, such as `/chat/completions`.
    #[must_use]
    pub fn upstream_path(&self) -> &str {
        self.upstream_path
    }

    /// The headers for this attempt, before the hub adds upstream auth and removes its control headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The headers for this attempt only.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers.to_mut()
    }

    /// The JSON body sent upstream, or `None` when it is empty or not JSON.
    pub fn body(&mut self) -> Option<&Value> {
        self.body.get()
    }

    /// The JSON body for a change to this attempt only.
    pub fn body_mut(&mut self) -> Option<&mut Value> {
        self.body.get_mut()
    }

    /// True when the body asks for a stream.
    pub fn stream(&mut self) -> bool {
        self.body()
            .and_then(|body| body.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Typed data for this request, shared with the other phases.
    #[must_use]
    pub fn state(&self) -> &Extensions {
        self.state
    }

    /// Typed data for this request, for a change.
    pub fn state_mut(&mut self) -> &mut Extensions {
        self.state
    }

    /// Sends `notification` to the Notifications inbox, from `ext:<extension name>`. The hub
    /// delivers it when this phase method returns, and drops it when notifications are off.
    pub fn notify(&self, notification: ExtNotification) {
        self.outbox.push(notification);
    }

    pub(crate) fn outbox(&self) -> &Outbox {
        &self.outbox
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    fn target() -> AttemptTarget<'static> {
        AttemptTarget {
            profile: "groq",
            model: "groq/llama",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            upstream_path: "/chat/completions",
        }
    }

    #[test]
    fn reads_the_attempt_fields() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let body = Bytes::from_static(br#"{"model":"llama","stream":true}"#);

        let mut ctx = AttemptCtx::new(target(), &headers, body, &mut state);

        assert_eq!(ctx.profile(), "groq");
        assert_eq!(ctx.model(), "groq/llama");
        assert_eq!(ctx.api(), Api::Chat);
        assert_eq!(ctx.client_path(), "/v1/chat/completions");
        assert_eq!(ctx.upstream_path(), "/chat/completions");
        assert!(ctx.headers().is_empty());
        assert!(ctx.stream());
        assert_eq!(ctx.body().unwrap()["model"], "llama");
    }

    #[test]
    fn untouched_parts_come_back_unchanged() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let raw = Bytes::from_static(br#"{"model":"llama"}"#);

        let ctx = AttemptCtx::new(target(), &headers, raw.clone(), &mut state);

        assert_eq!(ctx.into_parts(), (None, raw));
    }

    #[test]
    fn changes_apply_to_the_parts_and_the_shared_state() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let body = Bytes::from_static(br#"{"model":"llama"}"#);
        let mut ctx = AttemptCtx::new(target(), &headers, body, &mut state);

        ctx.headers_mut()
            .insert("x-a", HeaderValue::from_static("1"));
        ctx.body_mut().unwrap()["seed"] = json!(1);
        ctx.state_mut().insert("tag");
        assert!(ctx.state().get::<&str>().is_some());
        let (headers, body) = ctx.into_parts();

        assert_eq!(headers.unwrap()["x-a"], "1");
        assert_eq!(&body[..], br#"{"model":"llama","seed":1}"#);
        assert_eq!(state.get::<&str>(), Some(&"tag"));
    }

    #[test]
    fn a_body_without_json_does_not_stream() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();

        let mut ctx = AttemptCtx::new(target(), &headers, Bytes::new(), &mut state);

        assert!(!ctx.stream());
        assert!(ctx.body_mut().is_none());
    }
}

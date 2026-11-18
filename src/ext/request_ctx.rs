use std::borrow::Cow;

use axum::http::{Extensions, HeaderMap, Method};
use bytes::Bytes;
use serde_json::Value;

use crate::ext::{Api, JsonBody};

/// The `request` phase view of one `/v1/*` request, as the client sent it.
#[derive(Debug)]
pub struct RequestCtx<'a> {
    method: &'a Method,
    path: &'a str,
    headers: Cow<'a, HeaderMap>,
    body: JsonBody,
    state: &'a mut Extensions,
}

impl<'a> RequestCtx<'a> {
    pub(crate) fn new(
        method: &'a Method,
        path: &'a str,
        headers: &'a HeaderMap,
        body: Bytes,
        state: &'a mut Extensions,
    ) -> RequestCtx<'a> {
        RequestCtx {
            method,
            path,
            headers: Cow::Borrowed(headers),
            body: JsonBody::new(body),
            state,
        }
    }

    /// The headers and body to continue with. Headers are `None` when no extension changed them.
    pub(crate) fn into_parts(self) -> (Option<HeaderMap>, Bytes) {
        let headers = match self.headers {
            Cow::Owned(changed) => Some(changed),
            Cow::Borrowed(_) => None,
        };
        (headers, self.body.into_bytes())
    }

    /// The HTTP method.
    #[must_use]
    pub fn method(&self) -> &Method {
        self.method
    }

    /// The client path, with any query string.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path
    }

    /// The client API, read from the path.
    #[must_use]
    pub fn api(&self) -> Api {
        Api::from_path(self.path)
    }

    /// The request headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// The request headers for a change. Changed headers go to every attempt.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers.to_mut()
    }

    /// The JSON body, or `None` when it is empty or not JSON. The hub parses it on the first call.
    pub fn body(&mut self) -> Option<&Value> {
        self.body.get()
    }

    /// The JSON body for a change. The hub serializes the body again only after a call to this method.
    pub fn body_mut(&mut self) -> Option<&mut Value> {
        self.body.get_mut()
    }

    /// The `model` field of the body, such as `copilot/gpt-5`.
    pub fn model(&mut self) -> Option<&str> {
        self.body()?.get("model")?.as_str()
    }

    /// True when the body asks for a stream.
    pub fn stream(&mut self) -> bool {
        self.body()
            .and_then(|body| body.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// Typed data for this request. The later phases read the same data.
    #[must_use]
    pub fn state(&self) -> &Extensions {
        self.state
    }

    /// Typed data for this request, for a change.
    pub fn state_mut(&mut self) -> &mut Extensions {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    #[test]
    fn untouched_headers_and_body_come_back_unchanged() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let raw = Bytes::from_static(br#"{"model":"a/b"}"#);

        let ctx = RequestCtx::new(
            &Method::POST,
            "/v1/chat/completions",
            &headers,
            raw.clone(),
            &mut state,
        );

        assert_eq!(ctx.into_parts(), (None, raw));
    }

    #[test]
    fn reads_the_request_fields() {
        let mut headers = HeaderMap::new();
        headers.insert("x-team", HeaderValue::from_static("a"));
        let mut state = Extensions::new();
        let body = Bytes::from_static(br#"{"model":"copilot/gpt-5","stream":true}"#);

        let mut ctx = RequestCtx::new(
            &Method::POST,
            "/v1/responses?x=1",
            &headers,
            body,
            &mut state,
        );

        assert_eq!(ctx.method(), Method::POST);
        assert_eq!(ctx.path(), "/v1/responses?x=1");
        assert_eq!(ctx.api(), Api::Responses);
        assert_eq!(ctx.headers()["x-team"], "a");
        assert_eq!(ctx.model(), Some("copilot/gpt-5"));
        assert!(ctx.stream());
    }

    #[test]
    fn changed_headers_body_and_state_are_kept() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let body = Bytes::from_static(br#"{"model":"a/b","temperature":1}"#);
        let mut ctx = RequestCtx::new(
            &Method::POST,
            "/v1/chat/completions",
            &headers,
            body,
            &mut state,
        );

        ctx.headers_mut()
            .insert("x-team", HeaderValue::from_static("b"));
        ctx.body_mut().unwrap()["temperature"] = json!(0);
        ctx.state_mut().insert(7_u8);
        assert_eq!(ctx.state().get::<u8>(), Some(&7));
        let (headers, body) = ctx.into_parts();

        assert_eq!(headers.unwrap()["x-team"], "b");
        assert_eq!(&body[..], br#"{"model":"a/b","temperature":0}"#);
        assert_eq!(state.get::<u8>(), Some(&7));
    }

    #[test]
    fn a_body_without_json_has_no_model_and_no_stream() {
        let headers = HeaderMap::new();
        let mut state = Extensions::new();
        let mut ctx = RequestCtx::new(
            &Method::GET,
            "/v1/models",
            &headers,
            Bytes::new(),
            &mut state,
        );

        assert_eq!(ctx.model(), None);
        assert!(!ctx.stream());
    }
}

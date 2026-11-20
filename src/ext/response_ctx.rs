use axum::http::{Extensions, HeaderMap, StatusCode};
use serde_json::Value;

use crate::ext::Api;

/// The `response` phase view of one upstream reply, before the hub delivers it or tries the next
/// fallback.
#[derive(Debug)]
pub struct ResponseCtx<'a> {
    target: ResponseTarget<'a>,
    status: StatusCode,
    headers: &'a mut HeaderMap,
    error_body: Option<Value>,
    error_body_changed: bool,
    state: &'a mut Extensions,
}

/// Where a reply came from, for [`ResponseCtx::new`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResponseTarget<'a> {
    pub profile: &'a str,
    pub model: &'a str,
    pub api: Api,
    pub client_path: &'a str,
    pub copilot: bool,
    pub last_attempt: bool,
}

impl<'a> ResponseCtx<'a> {
    pub(crate) fn new(
        target: ResponseTarget<'a>,
        status: StatusCode,
        headers: &'a mut HeaderMap,
        state: &'a mut Extensions,
    ) -> ResponseCtx<'a> {
        ResponseCtx {
            target,
            status,
            headers,
            error_body: None,
            error_body_changed: false,
            state,
        }
    }

    pub(crate) fn set_error_body(&mut self, body: Option<Value>) {
        self.error_body = body;
    }

    /// The status to deliver, and the error body when an extension changed it.
    pub(crate) fn into_parts(self) -> (StatusCode, Option<Value>) {
        let body = self.error_body.filter(|_| self.error_body_changed);
        (self.status, body)
    }

    /// The profile that answered, such as `copilot`.
    #[must_use]
    pub fn profile(&self) -> &str {
        self.target.profile
    }

    /// The model that answered, as `<profile>/<model>`.
    #[must_use]
    pub fn model(&self) -> &str {
        self.target.model
    }

    /// The client API of the request.
    #[must_use]
    pub fn api(&self) -> Api {
        self.target.api
    }

    /// The client path, with any query string.
    #[must_use]
    pub fn client_path(&self) -> &str {
        self.target.client_path
    }

    /// True when the profile is a GitHub Copilot profile.
    #[must_use]
    pub fn is_copilot(&self) -> bool {
        self.target.copilot
    }

    /// True when no fallback remains after this attempt.
    #[must_use]
    pub fn is_last_attempt(&self) -> bool {
        self.target.last_attempt
    }

    /// The status the client gets.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Changes the status the client gets.
    pub fn set_status(&mut self, status: StatusCode) {
        self.status = status;
    }

    /// The reply headers the client gets.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.headers
    }

    /// The response headers the client gets.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers
    }

    /// The JSON error body, when the status is 400 or higher, an extension asked for it with
    /// [`crate::ext::Extension::wants_error_body`], and the body is JSON.
    #[must_use]
    pub fn error_body(&self) -> Option<&Value> {
        self.error_body.as_ref()
    }

    /// The JSON error body for a change. The client gets the changed body.
    pub fn error_body_mut(&mut self) -> Option<&mut Value> {
        let body = self.error_body.as_mut()?;
        self.error_body_changed = true;
        Some(body)
    }

    /// Replaces the reply body with `body`, also when the upstream body was not JSON or was not read.
    /// The caller sets any header the new body needs, such as `content-type`.
    pub fn replace_error_body(&mut self, body: Value) {
        self.error_body = Some(body);
        self.error_body_changed = true;
    }

    /// The `error.code` of the error body, when there is one.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        self.error_body()?.pointer("/error/code")?.as_str()
    }

    /// The message of the error body: `error.message`, a string `error`, `message` or `detail`.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        error_message_of(self.error_body()?)
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
}

/// The message of a JSON error body in the `OpenAI`, Anthropic, llama.cpp or `FastAPI` shape.
pub(crate) fn error_message_of(body: &Value) -> Option<&str> {
    ["/error/message", "/error", "/message", "/detail"]
        .iter()
        .find_map(|pointer| body.pointer(pointer).and_then(Value::as_str))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    fn target() -> ResponseTarget<'static> {
        ResponseTarget {
            profile: "copilot",
            model: "copilot/gpt-5",
            api: Api::Responses,
            client_path: "/v1/responses",
            copilot: true,
            last_attempt: true,
        }
    }

    #[test]
    fn reads_the_reply_fields() {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();

        let ctx = ResponseCtx::new(target(), StatusCode::OK, &mut headers, &mut state);

        assert_eq!(ctx.profile(), "copilot");
        assert_eq!(ctx.model(), "copilot/gpt-5");
        assert_eq!(ctx.api(), Api::Responses);
        assert_eq!(ctx.client_path(), "/v1/responses");
        assert!(ctx.is_last_attempt());
        assert!(ctx.is_copilot());
        assert_eq!(ctx.status(), StatusCode::OK);
        assert!(ctx.headers().is_empty());
        assert!(ctx.error_body().is_none());
        assert!(ctx.error_code().is_none());
        assert!(ctx.state().is_empty());
    }

    #[test]
    fn an_unchanged_error_body_is_not_returned() {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let mut ctx = ResponseCtx::new(target(), StatusCode::BAD_REQUEST, &mut headers, &mut state);

        ctx.set_error_body(Some(json!({"error": {"code": "x"}})));
        assert_eq!(ctx.error_code(), Some("x"));
        assert_eq!(ctx.error_message(), None);

        assert_eq!(ctx.into_parts(), (StatusCode::BAD_REQUEST, None));
    }

    #[test]
    fn the_error_message_is_read_from_each_known_shape() {
        for body in [
            json!({"error": {"message": "m"}}),
            json!({"error": "m"}),
            json!({"message": "m"}),
            json!({"detail": "m"}),
        ] {
            assert_eq!(error_message_of(&body), Some("m"), "{body}");
        }
        assert_eq!(error_message_of(&json!({"error": {"code": 1}})), None);
    }

    #[test]
    fn changes_to_status_headers_body_and_state_are_kept() {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let mut ctx = ResponseCtx::new(target(), StatusCode::BAD_REQUEST, &mut headers, &mut state);
        ctx.set_error_body(Some(json!({"error": {"code": "x"}})));

        ctx.set_status(StatusCode::TOO_MANY_REQUESTS);
        ctx.headers_mut()
            .insert("x-a", HeaderValue::from_static("1"));
        ctx.error_body_mut().unwrap()["error"]["code"] = json!("y");
        ctx.state_mut().insert(1_u8);

        assert_eq!(
            ctx.into_parts(),
            (
                StatusCode::TOO_MANY_REQUESTS,
                Some(json!({"error": {"code": "y"}}))
            )
        );
        assert_eq!(headers["x-a"], "1");
        assert_eq!(state.get::<u8>(), Some(&1));
    }

    #[test]
    fn without_an_error_body_there_is_nothing_to_change() {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let mut ctx = ResponseCtx::new(target(), StatusCode::OK, &mut headers, &mut state);

        assert!(ctx.error_body_mut().is_none());
    }
}

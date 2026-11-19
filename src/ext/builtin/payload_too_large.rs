use axum::http::{HeaderValue, StatusCode, header};
use serde_json::{Value, json};

use crate::ext::{Api, Extension, ResponseCtx, ResponseFlow};

/// The message clients match to compact the conversation and retry, such as opencode and Claude Code.
const MESSAGE: &str = "prompt is too long: the upstream rejected the request body as too large \
(HTTP 413). Compact the conversation and retry.";
/// The code `OpenAI` clients such as opencode and Codex read as a context overflow.
const OPENAI_CODE: &str = "context_length_exceeded";
/// The Anthropic error type for a request over the size limit.
const ANTHROPIC_TYPE: &str = "request_too_large";

/// Rewrites an upstream 413 into the context overflow error of the client's API, so the client
/// compacts instead of failing. An upstream such as Copilot sends a plain text 413 body.
pub(crate) struct PayloadTooLarge;

impl PayloadTooLarge {
    fn body(api: Api) -> Option<Value> {
        match api {
            Api::Chat | Api::Responses => Some(json!({ "error": {
                "message": MESSAGE,
                "type": "invalid_request_error",
                "code": OPENAI_CODE,
            }})),
            Api::Messages => Some(json!({
                "type": "error",
                "error": { "type": ANTHROPIC_TYPE, "message": MESSAGE },
            })),
            Api::Other => None,
        }
    }
}

impl Extension for PayloadTooLarge {
    fn name(&self) -> &'static str {
        "payload-too-large"
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        if ctx.status() != StatusCode::PAYLOAD_TOO_LARGE {
            return ResponseFlow::Continue;
        }
        if let Some(body) = Self::body(ctx.api()) {
            ctx.replace_error_body(body);
            let headers = ctx.headers_mut();
            headers.remove(header::CONTENT_ENCODING);
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        ResponseFlow::Continue
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Extensions, HeaderMap};

    use super::*;
    use crate::ext::ResponseTarget;

    fn run(api: Api, status: StatusCode, mut headers: HeaderMap) -> (Option<Value>, HeaderMap) {
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "copilot",
            model: "copilot/claude-sonnet-5-5",
            api,
            client_path: "/v1/chat/completions",
            copilot: true,
            last_attempt: true,
        };
        let mut ctx = ResponseCtx::new(target, status, &mut headers, &mut state);
        assert!(!PayloadTooLarge.wants_error_body(&ctx));
        assert_eq!(
            PayloadTooLarge.on_response(&mut ctx),
            ResponseFlow::Continue
        );
        let (delivered, body) = ctx.into_parts();
        assert_eq!(delivered, status, "the status stays");
        (body, headers)
    }

    fn gzip_text() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers
    }

    #[test]
    fn chat_and_responses_get_the_openai_context_code() {
        for api in [Api::Chat, Api::Responses] {
            let (body, headers) = run(api, StatusCode::PAYLOAD_TOO_LARGE, gzip_text());

            let body = body.unwrap();
            assert_eq!(body["error"]["code"], OPENAI_CODE);
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("prompt is too long")
            );
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
            assert!(!headers.contains_key(header::CONTENT_ENCODING));
        }
    }

    #[test]
    fn messages_get_the_anthropic_error_shape() {
        let (body, _) = run(
            Api::Messages,
            StatusCode::PAYLOAD_TOO_LARGE,
            HeaderMap::new(),
        );

        let body = body.unwrap();
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], ANTHROPIC_TYPE);
    }

    #[test]
    fn other_apis_and_statuses_are_left_alone() {
        for (api, status) in [
            (Api::Other, StatusCode::PAYLOAD_TOO_LARGE),
            (Api::Chat, StatusCode::BAD_REQUEST),
        ] {
            let (body, headers) = run(api, status, gzip_text());

            assert_eq!(body, None);
            assert_eq!(headers, gzip_text());
        }
    }
}

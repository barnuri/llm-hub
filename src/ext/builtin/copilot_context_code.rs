use crate::ext::{Api, Extension, ResponseCtx, ResponseFlow};

/// The code Copilot sends when a prompt is over the model's input limit.
const COPILOT_CONTEXT_CODE: &str = "model_max_prompt_tokens_exceeded";
/// The code `OpenAI` clients such as Codex read to compact the conversation.
const OPENAI_CONTEXT_CODE: &str = "context_length_exceeded";

/// Renames Copilot's Responses context overflow code to the `OpenAI` one. Status and message stay.
pub(crate) struct CopilotContextCode;

impl CopilotContextCode {
    fn applies(ctx: &ResponseCtx<'_>) -> bool {
        ctx.is_copilot() && ctx.api() == Api::Responses && ctx.status().as_u16() == 400
    }
}

impl Extension for CopilotContextCode {
    fn name(&self) -> &'static str {
        "copilot-context-code"
    }

    fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        Self::applies(ctx)
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        if Self::applies(ctx)
            && ctx.error_code() == Some(COPILOT_CONTEXT_CODE)
            && let Some(code) = ctx
                .error_body_mut()
                .and_then(|body| body.pointer_mut("/error/code"))
        {
            *code = OPENAI_CONTEXT_CODE.into();
        }
        ResponseFlow::Continue
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Extensions, HeaderMap, StatusCode};
    use serde_json::{Value, json};

    use super::*;
    use crate::ext::ResponseTarget;

    fn run(copilot: bool, api: Api, status: StatusCode, body: Value) -> (bool, Option<Value>) {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "copilot",
            model: "copilot/gpt-5",
            api,
            client_path: "/v1/responses",
            copilot,
            last_attempt: true,
        };
        let mut ctx = ResponseCtx::new(target, status, &mut headers, &mut state);
        let wants = CopilotContextCode.wants_error_body(&ctx);
        ctx.set_error_body(Some(body));
        assert_eq!(
            CopilotContextCode.on_response(&mut ctx),
            ResponseFlow::Continue
        );
        (wants, ctx.into_parts().1)
    }

    fn rejection(code: &str) -> Value {
        json!({"error": {"code": code, "message": "prompt is too long: 1200 tokens > 900 maximum"}})
    }

    #[test]
    fn copilot_context_code_is_renamed_and_the_message_stays() {
        let (wants, body) = run(
            true,
            Api::Responses,
            StatusCode::BAD_REQUEST,
            rejection(COPILOT_CONTEXT_CODE),
        );

        assert!(wants);
        assert_eq!(body, Some(rejection(OPENAI_CONTEXT_CODE)));
    }

    #[test]
    fn another_code_is_left_alone() {
        let (wants, body) = run(
            true,
            Api::Responses,
            StatusCode::BAD_REQUEST,
            rejection("invalid_request_error"),
        );

        assert!(wants);
        assert_eq!(body, None);
    }

    #[test]
    fn other_profiles_apis_and_statuses_are_not_read() {
        for (copilot, api, status) in [
            (false, Api::Responses, StatusCode::BAD_REQUEST),
            (true, Api::Chat, StatusCode::BAD_REQUEST),
            (true, Api::Responses, StatusCode::TOO_MANY_REQUESTS),
        ] {
            let (wants, body) = run(copilot, api, status, rejection(COPILOT_CONTEXT_CODE));

            assert!(!wants);
            assert_eq!(body, None);
        }
    }

    #[test]
    fn a_body_without_an_error_code_is_left_alone() {
        let (_, body) = run(
            true,
            Api::Responses,
            StatusCode::BAD_REQUEST,
            json!({"error": "plain"}),
        );

        assert_eq!(body, None);
    }
}

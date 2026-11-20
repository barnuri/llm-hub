use axum::http::StatusCode;

use super::AttemptNote;
use crate::ext::{Extension, ResponseCtx, ResponseFlow};

/// Lowercase parts of the 400 messages local servers (llama.cpp, llama-swap, LM Studio, Ollama)
/// send while a model loads or fails to start. Another model can answer the same request.
const NOT_READY: [&str; 9] = [
    "failed to initialize samplers",
    "loading model",
    "model is loading",
    "model is not loaded",
    "no models loaded",
    "failed to load model",
    "error loading model",
    "model unloaded",
    "model was unloaded",
];

/// Tries the next fallback when a local server answers 400 because its model is not ready, so
/// the request is not failed for a reason that is not in it. A genuine 400 still stops the chain.
pub(crate) struct LocalNotReady;

impl LocalNotReady {
    fn not_ready(message: &str) -> bool {
        let lower = message.to_lowercase();
        NOT_READY.iter().any(|part| lower.contains(part))
    }
}

impl Extension for LocalNotReady {
    fn name(&self) -> &'static str {
        "local-not-ready"
    }

    fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        ctx.status() == StatusCode::BAD_REQUEST && !ctx.is_last_attempt()
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        if ctx.status() != StatusCode::BAD_REQUEST || ctx.is_last_attempt() {
            return ResponseFlow::Continue;
        }
        let Some(message) = ctx.error_message().filter(|m| Self::not_ready(m)) else {
            return ResponseFlow::Continue;
        };
        let note: String = message.chars().take(160).collect();
        ctx.state_mut()
            .insert(AttemptNote(format!("model not ready: {note}")));
        ResponseFlow::RetryNext
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Extensions, HeaderMap};
    use serde_json::{Value, json};

    use super::*;
    use crate::ext::{Api, ResponseTarget};

    fn run(
        status: StatusCode,
        body: Value,
        last_attempt: bool,
    ) -> (bool, ResponseFlow, Extensions) {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "local",
            model: "local/qwen",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            copilot: false,
            last_attempt,
        };
        let mut ctx = ResponseCtx::new(target, status, &mut headers, &mut state);
        let wants = LocalNotReady.wants_error_body(&ctx);
        ctx.set_error_body(Some(body));
        let flow = LocalNotReady.on_response(&mut ctx);
        drop(ctx);
        (wants, flow, state)
    }

    #[test]
    fn a_model_that_is_not_ready_moves_to_the_next_fallback() {
        for message in [
            "Failed to initialize samplers",
            "Loading model",
            "No models loaded. Please load a model in the developer page",
        ] {
            let body = json!({"error": {"code": 400, "message": message, "type": "invalid_request_error"}});
            let (wants, flow, state) = run(StatusCode::BAD_REQUEST, body, false);

            assert!(wants);
            assert_eq!(flow, ResponseFlow::RetryNext, "{message}");
            let note = state.get::<AttemptNote>().unwrap();
            assert!(note.0.starts_with("model not ready: "), "{note:?}");
        }
        let (_, flow, _) = run(
            StatusCode::BAD_REQUEST,
            json!({"error": "Model unloaded."}),
            false,
        );
        assert_eq!(flow, ResponseFlow::RetryNext);
    }

    #[test]
    fn a_genuine_bad_request_stops_the_chain() {
        for body in [
            json!({"error": {"message": "messages: field required"}}),
            json!({"error": {"message": "the request exceeds the available context size"}}),
            json!({"unexpected": true}),
        ] {
            let (_, flow, state) = run(StatusCode::BAD_REQUEST, body, false);

            assert_eq!(flow, ResponseFlow::Continue);
            assert!(state.get::<AttemptNote>().is_none());
        }
    }

    #[test]
    fn the_last_attempt_and_other_statuses_are_left_alone() {
        let body = json!({"error": {"message": "Loading model"}});
        let (wants, flow, _) = run(StatusCode::BAD_REQUEST, body.clone(), true);
        assert!(!wants);
        assert_eq!(flow, ResponseFlow::Continue);

        let (wants, flow, _) = run(StatusCode::UNPROCESSABLE_ENTITY, body, false);
        assert!(!wants);
        assert_eq!(flow, ResponseFlow::Continue);
    }
}

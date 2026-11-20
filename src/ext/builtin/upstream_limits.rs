use super::AttemptNote;
use crate::ext::{Extension, ResponseCtx, ResponseFlow};
use crate::services::rate_limit::{BodyHint, CooldownHint, CooldownScope, body_hint};

/// Reads what a 400, 402, 403 or 429 body says about the limit it hit. An out-of-credits reply
/// cools the whole profile and tries the next fallback. A reset time in a 429 body cools the
/// model until then. The attempt loop applies the [`CooldownHint`] it leaves in the request state.
pub(crate) struct UpstreamLimits;

impl UpstreamLimits {
    fn applies(status: u16) -> bool {
        matches!(status, 400 | 402 | 403 | 429)
    }
}

impl Extension for UpstreamLimits {
    fn name(&self) -> &'static str {
        "upstream-limits"
    }

    fn wants_error_body(&self, ctx: &ResponseCtx<'_>) -> bool {
        Self::applies(ctx.status().as_u16())
    }

    fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
        let status = ctx.status().as_u16();
        if !Self::applies(status) {
            return ResponseFlow::Continue;
        }
        let Some(hint) = ctx.error_body().and_then(|body| body_hint(status, body)) else {
            return ResponseFlow::Continue;
        };
        match hint {
            BodyHint::OutOfCredits(message) => {
                let reason = format!("out of credits: {message}");
                ctx.state_mut().insert(AttemptNote(reason.clone()));
                ctx.state_mut().insert(CooldownHint {
                    scope: CooldownScope::Profile,
                    wait: None,
                    reason,
                });
                ResponseFlow::RetryNext
            }
            BodyHint::RetryAfter(wait) => {
                ctx.state_mut().insert(CooldownHint {
                    scope: CooldownScope::Model,
                    wait: Some(wait),
                    reason: format!("rate limited: the body says retry in {} s", wait.as_secs()),
                });
                ResponseFlow::Continue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Extensions, HeaderMap, StatusCode};
    use serde_json::{Value, json};

    use super::*;
    use crate::ext::{Api, ResponseTarget};

    fn run(status: u16, body: Option<Value>) -> (bool, ResponseFlow, Extensions) {
        let mut headers = HeaderMap::new();
        let mut state = Extensions::new();
        let target = ResponseTarget {
            profile: "openai",
            model: "openai/gpt-5",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            copilot: false,
            last_attempt: false,
        };
        let status = StatusCode::from_u16(status).unwrap();
        let mut ctx = ResponseCtx::new(target, status, &mut headers, &mut state);
        let wants = UpstreamLimits.wants_error_body(&ctx);
        ctx.set_error_body(body);
        let flow = UpstreamLimits.on_response(&mut ctx);
        drop(ctx);
        (wants, flow, state)
    }

    #[test]
    fn out_of_credits_cools_the_profile_and_moves_on() {
        let body = json!({"error": {"message": "You exceeded your current quota", "code": "insufficient_quota"}});
        let (wants, flow, state) = run(429, Some(body));

        assert!(wants);
        assert_eq!(flow, ResponseFlow::RetryNext);
        let hint = state.get::<CooldownHint>().unwrap();
        assert_eq!((hint.scope, hint.wait), (CooldownScope::Profile, None));
        assert!(hint.reason.starts_with("out of credits: You exceeded"));
        assert!(state.get::<AttemptNote>().is_some());
    }

    #[test]
    fn a_body_reset_cools_the_model_without_changing_the_flow() {
        let body = json!({"error": {"message": "Please try again in 20s."}});
        let (_, flow, state) = run(429, Some(body));

        assert_eq!(flow, ResponseFlow::Continue);
        let hint = state.get::<CooldownHint>().unwrap();
        assert_eq!(
            (hint.scope, hint.wait),
            (CooldownScope::Model, Some(Duration::from_secs(20)))
        );
    }

    #[test]
    fn other_replies_leave_no_hint() {
        for (status, body) in [
            (429, None),
            (400, Some(json!({"error": {"message": "bad schema"}}))),
            (500, Some(json!({"error": {"code": "insufficient_quota"}}))),
        ] {
            let (wants, flow, state) = run(status, body);

            assert_eq!(wants, status != 500);
            assert_eq!(flow, ResponseFlow::Continue);
            assert!(state.get::<CooldownHint>().is_none());
        }
    }
}

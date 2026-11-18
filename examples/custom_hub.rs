//! llm-hub as a library: your own binary with your own extension, route and middleware.
//! Run it with `cargo run --example custom_hub`. It reads `.env` like the `llm-hub` binary.

use std::sync::atomic::{AtomicU64, Ordering};

use llm_hub::Hub;
use llm_hub::axum::Router;
use llm_hub::axum::routing::get;
use llm_hub::ext::{AttemptCtx, CompleteCtx, Extension, Flow, RequestCtx};

/// Rejects requests without an `x-team` header, sends the team upstream, and counts failures.
#[derive(Default)]
struct TeamGate {
    failures: AtomicU64,
}

#[derive(Clone)]
struct Team(String);

impl Extension for TeamGate {
    fn name(&self) -> &str {
        "team-gate"
    }

    fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
        let Some(team) = ctx.headers().get("x-team").and_then(|v| v.to_str().ok()) else {
            return Flow::reject(401, "the x-team header is required");
        };
        let team = Team(team.to_owned());
        ctx.state_mut().insert(team);
        Flow::Continue
    }

    fn on_attempt(&self, ctx: &mut AttemptCtx<'_>) -> Flow {
        let team = ctx.state().get::<Team>().cloned();
        if let (Some(team), Some(body)) = (team, ctx.body_mut()) {
            body["user"] = team.0.into();
        }
        Flow::Continue
    }

    fn on_complete(&self, ctx: &CompleteCtx<'_>) {
        if ctx.status() >= 400 {
            let total = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!(
                "{} failed with {} ({total} failures so far)",
                ctx.model(),
                ctx.status()
            );
        }
    }
}

#[tokio::main]
async fn main() {
    let routes = Router::new().route("/team-gate", get(|| async { "team-gate is on" }));
    Hub::new()
        .extension(TeamGate::default())
        .routes(routes)
        .router(|router| router.layer(tower_http::trace::TraceLayer::new_for_http()))
        .run()
        .await;
}

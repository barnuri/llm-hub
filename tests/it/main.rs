//! One integration-test binary: one link step instead of fourteen. nextest still runs every test in its own process.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests fail by panicking"
)]

#[path = "../support/hub.rs"]
mod hub;

mod anthropic_messages;
mod copilot_admin;
mod fallback_resilience;
mod insights_api;
mod local_serving;
mod local_serving_mistralrs;
mod local_serving_watchdog;
mod managed_process;
mod model_health;
mod models_aggregation;
mod proxy_fallback;
mod review_routing;
mod smart_auto_e2e;
mod transforms;

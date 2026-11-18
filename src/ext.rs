//! Extension points of llm-hub. An [`Extension`] runs in fixed phases: `http_request` and
//! `http_response` on every route, `request` once per `/v1/*` request, `attempt` once per upstream
//! attempt, `response` once per upstream reply, `complete` once the outcome is recorded, and
//! `config_change` when a setting is saved. Every phase method has a no-op default.

mod api;
mod attempt_ctx;
pub(crate) mod builtin;
mod complete_ctx;
mod config_change_ctx;
mod extension;
mod extension_set;
mod flow;
pub(crate) mod hooks;
mod http_ctx;
mod json_body;
mod request_ctx;
mod response_ctx;
mod response_flow;
pub(crate) mod rules;

pub use api::Api;
pub use attempt_ctx::AttemptCtx;
pub(crate) use attempt_ctx::AttemptTarget;
pub use complete_ctx::CompleteCtx;
pub use config_change_ctx::{ConfigChangeCtx, ConfigKind};
pub use extension::Extension;
pub(crate) use extension_set::ExtensionSet;
pub use flow::Flow;
pub use http_ctx::{HttpRequestCtx, HttpResponseCtx};
pub(crate) use json_body::JsonBody;
pub use request_ctx::RequestCtx;
pub use response_ctx::ResponseCtx;
pub(crate) use response_ctx::ResponseTarget;
pub use response_flow::ResponseFlow;

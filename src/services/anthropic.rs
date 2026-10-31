//! Anthropic Messages API translation, in both directions.
//!
//! The hub speaks `OpenAI` chat-completions to every upstream. `/v1/messages`
//! therefore translates on the way in (`request`) and back out (`response`),
//! and reuses the ordinary proxy fallback loop in between. `native_reply`
//! repairs the opposite case: a Copilot Claude model answering
//! `/chat/completions` in Anthropic shape. The `responses_*` modules bridge a
//! Copilot model that serves only `/responses`.

pub mod native_reply;
pub mod request;
pub mod response;
pub mod responses_message;
pub mod responses_request;
pub mod responses_stream;
pub mod stream;

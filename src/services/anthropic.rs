//! Anthropic Messages API translation, in both directions.
//!
//! The hub speaks `OpenAI` chat-completions to every upstream. `/v1/messages`
//! therefore translates on the way in (`request`) and back out (`response`),
//! and reuses the ordinary proxy fallback loop in between.

pub mod request;
pub mod response;
pub mod stream;

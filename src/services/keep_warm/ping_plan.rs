use axum::http::HeaderValue;
use bytes::Bytes;

use crate::schemas::model_id::ModelId;

/// One keep-warm ping that is due: the stored ping body and the stream it belongs to.
pub(crate) struct PingPlan {
    pub(crate) worker: String,
    pub(crate) stream_key: String,
    pub(crate) model_id: ModelId,
    pub(crate) body: Bytes,
    /// The client's `anthropic-beta` header, which lets the upstream accept beta body fields.
    pub(crate) beta: Option<HeaderValue>,
}

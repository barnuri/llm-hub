use bytes::Bytes;

use super::PingTarget;
use crate::schemas::model_id::ModelId;

/// One keep-warm ping that is due: the stored ping body and the stream it belongs to.
pub(crate) struct PingPlan {
    pub(crate) worker: String,
    pub(crate) stream_key: String,
    pub(crate) model_id: ModelId,
    pub(crate) body: Bytes,
    /// Where the ping goes: the path, the API shape and the client's `anthropic-beta` header.
    pub(crate) target: PingTarget,
}

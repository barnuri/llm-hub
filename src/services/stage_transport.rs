use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::schemas::model_id::ModelId;
use crate::schemas::stage_error::StageError;
use crate::schemas::stage_reply::StageReply;

/// Anything that answers one review stage on a concrete model.
pub trait StageTransport: Send + Sync {
    /// Sends `body` to `model` and returns the completion, failing past `cap_bytes`.
    fn complete<'a>(
        &'a self,
        model: &'a ModelId,
        body: Value,
        cap_bytes: u64,
    ) -> BoxFuture<'a, Result<StageReply, StageError>>;
}

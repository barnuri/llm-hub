use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

use super::{KeepWarm, PingTarget};
use crate::schemas::model_id::ModelId;

/// One user request in flight for a worker. Dropping it ends the request.
pub(crate) struct KeepWarmLease {
    registry: Arc<KeepWarm>,
    worker: String,
    recorded: Option<Recorded>,
}

/// The accepted upstream body and its request start, held until the response completes.
struct Recorded {
    body: Bytes,
    model_id: ModelId,
    started: Instant,
    target: PingTarget,
}

impl KeepWarmLease {
    pub(super) fn new(registry: Arc<KeepWarm>, worker: &str) -> Self {
        Self {
            registry,
            worker: worker.to_string(),
            recorded: None,
        }
    }

    /// Keeps the body the upstream accepted, so the stream can be kept once the response completes.
    pub(crate) fn record_stream(
        &mut self,
        body: Bytes,
        model_id: ModelId,
        started: Instant,
        target: PingTarget,
    ) {
        self.recorded = Some(Recorded {
            body,
            model_id,
            started,
            target,
        });
    }

    /// Keeps the recorded stream when the response cached enough prompt tokens. The stream key is
    /// read on a blocking thread, so the response path does no parsing.
    pub(crate) fn complete(mut self, cached_tokens: u64) {
        let Some(recorded) = self.recorded.take() else {
            return;
        };
        let registry = Arc::clone(&self.registry);
        let worker = self.worker.clone();
        tokio::task::spawn_blocking(move || {
            let Some(stream_key) = recorded.target.shape.stream_key(&recorded.body) else {
                return;
            };
            registry.touch_with(
                (worker, stream_key),
                &recorded.body,
                &recorded.model_id,
                recorded.started,
                cached_tokens,
                recorded.target,
            );
        });
    }
}

impl Drop for KeepWarmLease {
    fn drop(&mut self) {
        self.registry.finish(&self.worker);
    }
}

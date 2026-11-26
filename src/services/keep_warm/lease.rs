use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;

use super::{CachedPrompt, KeepWarm, PingTarget, SkipEvent, SkipReason};
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
    pub(crate) fn complete(mut self, prompt: CachedPrompt) {
        self.settle(Settle::Keep(prompt));
    }

    /// Drops the recorded stream, as a completion with no cached tokens did, and logs it as
    /// unpaid: the model's cache discount cannot pay for the pings.
    pub(crate) fn complete_unpaid(mut self, cached_tokens: u64) {
        self.settle(Settle::Unpaid(cached_tokens));
    }

    /// Logs that the recorded stream is not kept. The lease stays, so the worker stays busy
    /// until it drops.
    pub(crate) fn not_kept(&mut self, reason: SkipReason) {
        self.settle(Settle::Skip(reason));
    }

    fn settle(&mut self, settle: Settle) {
        let Some(recorded) = self.recorded.take() else {
            return;
        };
        let registry = Arc::clone(&self.registry);
        let worker = self.worker.clone();
        tokio::task::spawn_blocking(move || {
            let Recorded {
                body,
                model_id,
                started,
                target,
            } = recorded;
            let Some(stream_key) = target.shape.stream_key(&body) else {
                let event =
                    SkipEvent::new(SkipReason::NotKeptNoStreamKey, &worker, None, &model_id)
                        .bytes(body.len());
                registry.note(event);
                return;
            };
            let event = |reason| {
                SkipEvent::new(reason, &worker, Some(&stream_key), &model_id).bytes(body.len())
            };
            match settle {
                Settle::Keep(prompt) => {
                    registry.touch_with(
                        (worker.clone(), stream_key.clone()),
                        &body,
                        &model_id,
                        started,
                        prompt,
                        target,
                    );
                }
                Settle::Unpaid(cached_tokens) => {
                    let unpaid = event(SkipReason::NotKeptUnpaid).tokens(cached_tokens);
                    registry.forget(&(worker.clone(), stream_key.clone()), unpaid);
                }
                Settle::Skip(reason) => registry.note(event(reason)),
            }
        });
    }
}

/// What happens to a recorded stream when its response ends.
enum Settle {
    /// Keep it, or drop it when it cached too few tokens.
    Keep(CachedPrompt),
    /// Drop it: pings cannot pay for themselves.
    Unpaid(u64),
    /// Leave any kept stream as it is and log the reason.
    Skip(SkipReason),
}

impl Drop for KeepWarmLease {
    fn drop(&mut self) {
        self.registry.finish(&self.worker);
    }
}

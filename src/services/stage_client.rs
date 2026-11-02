use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::dependencies::state::AppState;
use crate::routes::proxy;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::model_id::ModelId;
use crate::schemas::stage_error::StageError;
use crate::schemas::stage_reply::StageReply;
use crate::services::stage_transport::StageTransport;

/// The production transport: a concrete, non-streaming call through the proxy's pooled client.
#[derive(Clone)]
pub struct StageClient {
    state: AppState,
}

impl StageClient {
    /// The upstream statuses that carry a usable reply.
    const SUCCESS_STATUS: std::ops::Range<u16> = 200..300;

    /// A transport over the running hub state.
    #[must_use]
    pub fn new(state: AppState) -> StageClient {
        StageClient { state }
    }

    async fn call(
        &self,
        model: &ModelId,
        mut body: Value,
        cap_bytes: u64,
    ) -> Result<StageReply, StageError> {
        Self::reject_recursion(model)?;
        let sent_id = ModelId {
            profile: model.profile.clone(),
            model: self
                .state
                .upstream_model(&model.profile, &model.model)
                .map_or_else(|| model.model.clone(), |catalog| catalog.upstream_id),
        };
        if let Some(object) = body.as_object_mut() {
            object.insert("model".to_owned(), Value::String(sent_id.model.clone()));
        }
        let encoded = serde_json::to_vec(&body)
            .map_err(|e| StageError::Malformed(format!("stage body: {e}")))?;
        let response = proxy::send_stage(&self.state, &sent_id, Bytes::from(encoded))
            .await
            .map_err(StageError::Upstream)?;
        let status = response.status().as_u16();
        let raw = Self::read_capped(response, cap_bytes).await?;
        if !Self::SUCCESS_STATUS.contains(&status) {
            return Err(StageError::Upstream(format!("HTTP {status}")));
        }
        let completion: Value =
            serde_json::from_slice(&raw).map_err(|e| StageError::Malformed(e.to_string()))?;
        Ok(StageReply {
            completion,
            bytes: raw.len() as u64,
        })
    }

    /// Rejects a `hub/*` stage target, so a review stage can never re-enter Smart Auto.
    fn reject_recursion(model: &ModelId) -> Result<(), StageError> {
        if model.profile == HubSelector::NAMESPACE {
            return Err(StageError::Recursion(model.qualified()));
        }
        Ok(())
    }

    /// Reads a reply body, stopping as soon as it exceeds `cap` bytes.
    pub(crate) async fn read_capped(
        response: reqwest::Response,
        cap: u64,
    ) -> Result<Vec<u8>, StageError> {
        let mut stream = response.bytes_stream();
        let mut raw: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| StageError::Upstream(format!("read error: {e}")))?;
            if (raw.len() + chunk.len()) as u64 > cap {
                return Err(StageError::TooLarge);
            }
            raw.extend_from_slice(&chunk);
        }
        Ok(raw)
    }
}

impl StageTransport for StageClient {
    fn complete<'a>(
        &'a self,
        model: &'a ModelId,
        body: Value,
        cap_bytes: u64,
    ) -> BoxFuture<'a, Result<StageReply, StageError>> {
        Box::pin(self.call(model, body, cap_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stage_error_has_a_distinct_reason() {
        let reasons = [
            StageError::Recursion("hub/x".to_owned()).reason(),
            StageError::TooLarge.reason(),
            StageError::Upstream("HTTP 500".to_owned()).reason(),
            StageError::Malformed("eof".to_owned()).reason(),
            StageError::Timeout.reason(),
        ];
        let mut unique = reasons.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), reasons.len());
    }

    #[test]
    fn hub_selectors_are_rejected_as_stage_targets() {
        let hub_model = ModelId::parse("hub/smart-auto-balance").expect("parses");
        assert!(matches!(
            StageClient::reject_recursion(&hub_model),
            Err(StageError::Recursion(_))
        ));
        let concrete = ModelId::parse("copilot/gpt-5").expect("parses");
        assert_eq!(StageClient::reject_recursion(&concrete), Ok(()));
    }
}

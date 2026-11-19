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
use crate::services::split_choices::SplitChoices;
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
        let mut completion: Value =
            serde_json::from_slice(&raw).map_err(|e| StageError::Malformed(e.to_string()))?;
        SplitChoices::merge(&mut completion);
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
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::upstream_model::UpstreamModel;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const CAP: u64 = 64 * 1024;
    const CHAT_PATH: &str = "/chat/completions";

    fn client_for(base_url: &str) -> StageClient {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "acme"),
            ("LLM_HUB_ACME_BASE_URL", base_url),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        StageClient::new(state)
    }

    fn completion() -> Value {
        json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        })
    }

    fn model(raw: &str) -> ModelId {
        ModelId::parse(raw).expect("parses")
    }

    async fn upstream_replying(response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT_PATH))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_stage_call_returns_the_completion_and_its_byte_count() {
        let reply = serde_json::to_vec(&completion()).unwrap();
        let server = upstream_replying(
            ResponseTemplate::new(200).set_body_raw(reply.clone(), "application/json"),
        )
        .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!({"messages": []}), CAP)
            .await
            .unwrap();

        assert_eq!(result.completion, completion());
        assert_eq!(result.bytes, reply.len() as u64);
    }

    #[tokio::test]
    async fn a_stage_call_sends_the_upstream_id_of_a_listed_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT_PATH))
            .and(body_partial_json(json!({"model": "Vendor-M1"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server.uri());
        client.state.set_upstream_models(
            "acme",
            HashMap::from([(
                "m1".to_owned(),
                UpstreamModel {
                    upstream_id: "Vendor-M1".to_owned(),
                    native_messages: false,
                },
            )]),
        );

        let result = client
            .complete(
                &model("acme/m1"),
                json!({"model": "hub/x", "messages": []}),
                CAP,
            )
            .await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_stage_call_sends_the_bare_model_when_the_catalog_does_not_list_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT_PATH))
            .and(body_partial_json(json!({"model": "m1"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!({"messages": []}), CAP)
            .await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_an_object_is_sent_unchanged() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CHAT_PATH))
            .and(body_partial_json(json!(["raw"])))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!(["raw"]), CAP)
            .await;

        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_upstream_error() {
        let server =
            upstream_replying(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
                .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!({"messages": []}), CAP)
            .await;

        assert_eq!(result, Err(StageError::Upstream("HTTP 503".to_owned())));
    }

    #[tokio::test]
    async fn a_reply_over_the_byte_cap_is_too_large() {
        let server =
            upstream_replying(ResponseTemplate::new(200).set_body_json(completion())).await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!({"messages": []}), 8)
            .await;

        assert_eq!(result, Err(StageError::TooLarge));
    }

    #[tokio::test]
    async fn a_reply_that_is_not_json_is_malformed() {
        let server = upstream_replying(
            ResponseTemplate::new(200).set_body_raw("not json", "application/json"),
        )
        .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("acme/m1"), json!({"messages": []}), CAP)
            .await;

        assert!(
            matches!(result, Err(StageError::Malformed(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_unknown_profile_is_an_upstream_error() {
        let client = client_for("http://127.0.0.1:9");

        let result = client
            .complete(&model("other/m1"), json!({"messages": []}), CAP)
            .await;

        assert_eq!(
            result,
            Err(StageError::Upstream("unknown profile: other".to_owned()))
        );
    }

    #[tokio::test]
    async fn a_hub_target_is_rejected_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(completion()))
            .expect(0)
            .mount(&server)
            .await;
        let client = client_for(&server.uri());

        let result = client
            .complete(&model("hub/smart-auto-balance"), json!({}), CAP)
            .await;

        assert_eq!(
            result,
            Err(StageError::Recursion("hub/smart-auto-balance".to_owned()))
        );
    }

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

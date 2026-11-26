//! Native local-process probe for hub-owned models.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

use crate::services::local_serving::pool::LocalModelSnapshot;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeEvidence {
    pub ctx: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct Props {
    #[serde(default)]
    n_ctx: Option<u64>,
}

pub async fn probe(client: &reqwest::Client, model: &LocalModelSnapshot) -> NativeEvidence {
    let Some(port) = model.port else {
        return NativeEvidence::default();
    };
    let props = get::<Props>(client, &format!("http://127.0.0.1:{port}/props"))
        .await
        .ok();
    NativeEvidence {
        ctx: props.and_then(|value| value.n_ctx),
    }
}

async fn get<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let response = client
        .get(url)
        .timeout(READ_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if status != StatusCode::OK {
        return Err(status.to_string());
    }
    response
        .json::<T>()
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::local_serving::pool::LocalModelState;

    fn model(port: Option<u16>, state: LocalModelState) -> LocalModelSnapshot {
        LocalModelSnapshot {
            profile: "local".into(),
            id: "a".into(),
            state,
            port,
            pid: None,
            argv: Vec::new(),
            engine: None,
            started: None,
            last_used: std::time::Instant::now(),
            in_flight: 0,
            estimate_gb: 0.0,
        }
    }

    fn port_of(server: &MockServer) -> u16 {
        server
            .uri()
            .parse::<reqwest::Url>()
            .unwrap()
            .port()
            .unwrap()
    }

    async fn serve(route: &str, response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn missing_port_has_no_native_evidence_and_makes_no_request() {
        let model = model(None, LocalModelState::Stopped);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence::default()
        );
        assert_eq!(model.state, LocalModelState::Stopped);
    }

    #[tokio::test]
    async fn props_reads_the_native_context() {
        let server = serve(
            "/props",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"n_ctx": 65536})),
        )
        .await;
        let model = model(Some(port_of(&server)), LocalModelState::Running);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence { ctx: Some(65536) }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn props_without_n_ctx_has_no_context() {
        let server = serve(
            "/props",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
        )
        .await;
        let model = model(Some(port_of(&server)), LocalModelState::Running);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence::default()
        );
    }

    #[tokio::test]
    async fn props_404_means_native_server_is_not_available() {
        let server = serve("/props", ResponseTemplate::new(404)).await;
        let model = model(Some(port_of(&server)), LocalModelState::Running);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence::default()
        );
        let error = get::<Props>(&crate::services::http_client::client(), &format!("{}/props", server.uri()))
            .await
            .unwrap_err();
        assert!(error.contains("404"));
    }

    #[tokio::test]
    async fn props_rejects_malformed_json() {
        let server = serve(
            "/props",
            ResponseTemplate::new(200).set_body_string("not-json"),
        )
        .await;
        let model = model(Some(port_of(&server)), LocalModelState::Running);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence::default()
        );
    }

    #[derive(Deserialize)]
    struct Metrics {
        requests: u64,
    }

    #[derive(Deserialize)]
    struct Slot {
        id: u64,
    }

    #[tokio::test]
    async fn metrics_reader_accepts_a_successful_model_response() {
        let server = serve(
            "/metrics",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"requests": 3})),
        )
        .await;

        let metrics = get::<Metrics>(
            &crate::services::http_client::client(),
            &format!("{}/metrics", server.uri()),
        )
        .await
        .unwrap();
        assert_eq!(metrics.requests, 3);
    }

    #[tokio::test]
    async fn slots_reader_accepts_a_per_model_slots_response() {
        let server = serve(
            "/slots",
            ResponseTemplate::new(200).set_body_json(serde_json::json!([{"id": 7}])),
        )
        .await;

        let slots = get::<Vec<Slot>>(&crate::services::http_client::client(), &format!("{}/slots", server.uri()))
            .await
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].id, 7);
    }

    #[tokio::test]
    async fn stopped_model_probe_only_reads_props_and_never_starts_it() {
        let server = serve(
            "/props",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"n_ctx": 8192})),
        )
        .await;
        let model = model(Some(port_of(&server)), LocalModelState::Stopped);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence { ctx: Some(8192) }
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/props");
    }

    #[tokio::test]
    async fn missing_port_is_the_same_across_per_model_states() {
        for state in [
            LocalModelState::Stopped,
            LocalModelState::Starting,
            LocalModelState::Running,
            LocalModelState::Failed,
        ] {
            let model = model(None, state);
            assert_eq!(
                probe(&crate::services::http_client::client(), &model).await,
                NativeEvidence::default()
            );
            assert_eq!(model.port, None);
        }
    }

    #[tokio::test]
    async fn request_error_returns_default_evidence() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let model = model(Some(port), LocalModelState::Running);

        assert_eq!(
            probe(&crate::services::http_client::client(), &model).await,
            NativeEvidence::default()
        );
    }
}

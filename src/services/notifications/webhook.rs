//! The webhook sink: POSTs each notification as JSON to one URL.

use std::time::Duration;

use crate::schemas::notification::Notification;

/// Sends notifications to `LLM_HUB_NOTIFY_WEBHOOK_URL` through a pooled client.
#[derive(Debug, Clone)]
pub struct WebhookSink {
    client: reqwest::Client,
    url: String,
    timeout: Duration,
}

impl WebhookSink {
    /// A sink that posts to `url` with `client`, giving up after `timeout`.
    #[must_use]
    pub fn new(client: reqwest::Client, url: String, timeout: Duration) -> WebhookSink {
        WebhookSink {
            client,
            url,
            timeout,
        }
    }

    /// POSTs `notification` as JSON.
    ///
    /// # Errors
    /// Returns a message when the request fails or the answer is not 2xx.
    pub async fn send(&self, notification: &Notification) -> Result<(), String> {
        let response = self
            .client
            .post(&self.url)
            .timeout(self.timeout)
            .json(notification)
            .send()
            .await
            .map_err(|e| format!("webhook request failed: {e}"))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("webhook answered {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::notifications::desktop::test_notification;

    fn sink(url: String) -> WebhookSink {
        WebhookSink::new(crate::services::http_client::client(), url, Duration::from_secs(5))
    }

    #[tokio::test]
    async fn send_posts_the_notification_json() {
        let server = MockServer::start().await;
        let notification = test_notification();
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("content-type", "application/json"))
            .and(body_json(&notification))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let result = sink(format!("{}/hook", server.uri()))
            .send(&notification)
            .await;

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn send_reports_a_non_2xx_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let error = sink(server.uri())
            .send(&test_notification())
            .await
            .unwrap_err();

        assert_eq!(error, "webhook answered 503 Service Unavailable");
    }

    #[tokio::test]
    async fn send_reports_a_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let sink = WebhookSink::new(
            crate::services::http_client::client(),
            server.uri(),
            Duration::from_millis(50),
        );

        let error = sink.send(&test_notification()).await.unwrap_err();

        assert!(error.starts_with("webhook request failed:"), "{error}");
    }
}

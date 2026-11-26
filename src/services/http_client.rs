use std::sync::OnceLock;

/// A client builder that trusts the system root certificates.
///
/// Building a `reqwest` client with its built-in native roots reads and parses the whole system
/// certificate store every time. This loads the store once per process and hands each builder the
/// parsed certificates, so building a client costs microseconds instead of tens of milliseconds.
pub fn builder() -> reqwest::ClientBuilder {
    static ROOTS: OnceLock<Vec<reqwest::Certificate>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        rustls_native_certs::load_native_certs()
            .certs
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok())
            .collect()
    });
    roots.iter().fold(
        reqwest::Client::builder().tls_built_in_root_certs(false),
        |builder, root| builder.add_root_certificate(root.clone()),
    )
}

/// A client with the default settings and the cached system roots, for tests.
#[cfg(test)]
pub fn client() -> reqwest::Client {
    builder().build().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builder_makes_a_client_and_repeat_builds_work() {
        assert!(builder().build().is_ok());
        assert!(builder().build().is_ok());
    }

    #[tokio::test]
    async fn the_client_reaches_a_plain_http_server() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let response = client().get(server.uri()).send().await.unwrap();
        let body = response.text().await.unwrap();

        assert_eq!(body, "ok");
    }
}

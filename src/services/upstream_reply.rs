use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

/// A response whose body the hub already started reading, handed back whole.
pub(crate) fn rebuilt_response(
    status: StatusCode,
    version: reqwest::Version,
    headers: HeaderMap,
    body: impl Into<reqwest::Body>,
) -> reqwest::Response {
    let mut rebuilt = Response::new(body.into());
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    reqwest::Response::from(rebuilt)
}

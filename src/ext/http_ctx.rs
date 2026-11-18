use axum::http::{HeaderMap, Method, StatusCode};

use crate::ext::Api;

/// The `http_request` phase view of any request, before the hub checks the master key. It runs
/// on every route: `/v1/*`, `/api/*`, the UI and the health check.
#[derive(Debug)]
pub struct HttpRequestCtx<'a> {
    method: &'a Method,
    path: &'a str,
    headers: &'a mut HeaderMap,
}

impl<'a> HttpRequestCtx<'a> {
    pub(crate) fn new(
        method: &'a Method,
        path: &'a str,
        headers: &'a mut HeaderMap,
    ) -> HttpRequestCtx<'a> {
        HttpRequestCtx {
            method,
            path,
            headers,
        }
    }

    /// The HTTP method.
    #[must_use]
    pub fn method(&self) -> &Method {
        self.method
    }

    /// The request path, without the query string.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path
    }

    /// The client API, read from the path. `Api::Other` for every route that is not an inference
    /// route.
    #[must_use]
    pub fn api(&self) -> Api {
        Api::from_path(self.path)
    }

    /// The request headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.headers
    }

    /// The request headers, for a change.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers
    }
}

/// The `http_response` phase view of any response, as it leaves the hub. It runs on every route,
/// including a response an extension rejected with.
#[derive(Debug)]
pub struct HttpResponseCtx<'a> {
    method: &'a Method,
    path: &'a str,
    status: StatusCode,
    headers: &'a mut HeaderMap,
}

impl<'a> HttpResponseCtx<'a> {
    pub(crate) fn new(
        method: &'a Method,
        path: &'a str,
        status: StatusCode,
        headers: &'a mut HeaderMap,
    ) -> HttpResponseCtx<'a> {
        HttpResponseCtx {
            method,
            path,
            status,
            headers,
        }
    }

    /// The status of the request this response answers.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Changes the status the client gets.
    pub fn set_status(&mut self, status: StatusCode) {
        self.status = status;
    }

    /// The new status, when an extension changed it.
    pub(crate) fn changed_status(&self, original: StatusCode) -> Option<StatusCode> {
        (self.status != original).then_some(self.status)
    }

    /// The HTTP method of the request.
    #[must_use]
    pub fn method(&self) -> &Method {
        self.method
    }

    /// The request path, without the query string.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path
    }

    /// The client API, read from the path.
    #[must_use]
    pub fn api(&self) -> Api {
        Api::from_path(self.path)
    }

    /// The response headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        self.headers
    }

    /// The response headers, for a change.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn the_request_context_reads_and_changes_the_request() {
        let mut headers = HeaderMap::new();
        let mut ctx = HttpRequestCtx::new(&Method::POST, "/v1/responses", &mut headers);

        assert_eq!(ctx.method(), Method::POST);
        assert_eq!(ctx.path(), "/v1/responses");
        assert_eq!(ctx.api(), Api::Responses);
        assert!(ctx.headers().is_empty());
        ctx.headers_mut()
            .insert("x-a", HeaderValue::from_static("1"));

        assert_eq!(headers["x-a"], "1");
    }

    #[test]
    fn the_response_context_reads_and_changes_the_response() {
        let mut headers = HeaderMap::new();
        let mut ctx =
            HttpResponseCtx::new(&Method::GET, "/api/stats", StatusCode::OK, &mut headers);

        assert_eq!(ctx.method(), Method::GET);
        assert_eq!(ctx.path(), "/api/stats");
        assert_eq!(ctx.api(), Api::Other);
        assert_eq!(ctx.status(), StatusCode::OK);
        assert_eq!(ctx.changed_status(StatusCode::OK), None);
        ctx.set_status(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            ctx.changed_status(StatusCode::OK),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        ctx.headers_mut()
            .insert("x-b", HeaderValue::from_static("2"));
        assert_eq!(ctx.headers()["x-b"], "2");
    }
}

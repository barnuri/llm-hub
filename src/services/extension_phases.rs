//! Runs the extension phases on the request path. Each function changes nothing when no
//! extension acts, and reads or rebuilds a body only when one asks for it.

use axum::body::{Body, to_bytes};
use axum::http::{Extensions, HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use serde_json::Value;

use crate::consts::MAX_TRANSFORM_BUFFER_BYTES;
use crate::dependencies::state::AppState;
use crate::ext::{
    AttemptCtx, AttemptTarget, ConfigChangeCtx, ConfigKind, ExtensionSet, Flow, RequestCtx,
    ResponseCtx, ResponseFlow, ResponseTarget,
};
use crate::schemas::app_error::AppError;
use crate::services::upstream_reply::rebuilt_response;

/// The client request after the `request` phase: changed headers, if any, and the body to use.
pub(crate) async fn request_phase(
    set: &ExtensionSet,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
    state: &mut Extensions,
) -> Result<(Option<HeaderMap>, Bytes), AppError> {
    let mut ctx = RequestCtx::new(method, path, headers, body, state);
    rejected(set.request(&mut ctx))?;
    if let Some(hooks) = set.hooks() {
        rejected(hooks.request(&mut ctx).await)?;
    }
    Ok(ctx.into_parts())
}

/// One attempt after the `attempt` phase: changed headers, if any, and the body to send.
pub(crate) async fn attempt_phase(
    set: &ExtensionSet,
    target: AttemptTarget<'_>,
    headers: &HeaderMap,
    body: Bytes,
    state: &mut Extensions,
) -> Result<(Option<HeaderMap>, Bytes), AppError> {
    let mut ctx = AttemptCtx::new(target, headers, body, state);
    rejected(set.attempt(&mut ctx))?;
    if let Some(hooks) = set.hooks() {
        rejected(hooks.attempt(&mut ctx).await)?;
    }
    Ok(ctx.into_parts())
}

/// Runs `apply` between the `config_change` phase, which can stop the save, and the
/// `config_changed` phase, which runs only after a successful save.
///
/// # Errors
/// `AppError::Rejected` when an extension rejects the change, else what `apply` returns.
pub(crate) fn changing<T>(
    state: &AppState,
    kind: ConfigKind,
    subject: Option<&str>,
    apply: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    let extensions = state.extensions();
    let ctx = ConfigChangeCtx::new(kind, subject);
    rejected(extensions.config_change(&ctx))?;
    let saved = apply()?;
    extensions.config_changed(&ctx);
    Ok(saved)
}

fn rejected(flow: Flow) -> Result<(), AppError> {
    match flow {
        Flow::Continue => Ok(()),
        Flow::Reject { status, message } => Err(AppError::Rejected(status, message)),
    }
}

/// What the `response` phase changed for the client. The hub's own stats and limit learning keep
/// reading the upstream reply as it came.
#[derive(Debug, Default)]
pub(crate) struct ClientChanges {
    status: Option<StatusCode>,
    set_headers: HeaderMap,
    removed_headers: Vec<header::HeaderName>,
    body: Option<Bytes>,
}

impl ClientChanges {
    /// The same changes without the body change. Used when the hub translates the reply, so
    /// the upstream body shape is not the client's.
    pub(crate) fn without_body(mut self) -> ClientChanges {
        self.body = None;
        self
    }

    fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.set_headers.is_empty()
            && self.removed_headers.is_empty()
            && self.body.is_none()
    }

    /// `delivered` with these changes. A changed body first drains the delivered one, so the
    /// hub's stats still see the upstream reply.
    pub(crate) async fn apply(self, delivered: Response) -> Response {
        if self.is_empty() {
            return delivered;
        }
        let (mut parts, body) = delivered.into_parts();
        let body = match self.body {
            Some(changed) => {
                if let Err(e) = to_bytes(body, MAX_TRANSFORM_BUFFER_BYTES).await {
                    tracing::debug!("the replaced reply body did not drain: {e}");
                }
                parts.headers.remove(header::CONTENT_LENGTH);
                Body::from(changed)
            }
            None => body,
        };
        if let Some(status) = self.status {
            parts.status = status;
        }
        for name in &self.removed_headers {
            parts.headers.remove(name);
        }
        for name in self.set_headers.keys() {
            parts.headers.remove(name);
        }
        for (name, value) in &self.set_headers {
            parts.headers.append(name.clone(), value.clone());
        }
        Response::from_parts(parts, body)
    }
}

/// The upstream reply for the hub, whether to try the next fallback, and the changes for the client.
pub(crate) async fn response_phase(
    set: &ExtensionSet,
    target: ResponseTarget<'_>,
    response: reqwest::Response,
    state: &mut Extensions,
) -> (reqwest::Response, ResponseFlow, ClientChanges) {
    let status = response.status();
    let original = response.headers().clone();
    let mut headers = original.clone();
    let mut ctx = ResponseCtx::new(target, status, &mut headers, state);
    let readable = status.as_u16() >= 400 && !is_compressed(ctx.headers());
    let (response, flow) = if readable && set.wants_error_body(&ctx) {
        let version = response.version();
        match read_capped(response, MAX_TRANSFORM_BUFFER_BYTES).await {
            Ok(raw) => {
                ctx.set_error_body(serde_json::from_slice::<Value>(&raw).ok());
                let flow = set.response(&mut ctx);
                (
                    rebuilt_response(status, version, original.clone(), raw),
                    flow,
                )
            }
            Err(replay) => {
                let flow = set.response(&mut ctx);
                (
                    rebuilt_response(status, version, original.clone(), replay),
                    flow,
                )
            }
        }
    } else {
        let flow = set.response(&mut ctx);
        (response, flow)
    };
    let (new_status, changed_body) = ctx.into_parts();
    let changes = ClientChanges {
        status: (new_status != status).then_some(new_status),
        set_headers: changed_headers(&original, &headers),
        removed_headers: original
            .keys()
            .filter(|name| !headers.contains_key(*name))
            .cloned()
            .collect(),
        body: changed_body.and_then(|value| serde_json::to_vec(&value).ok().map(Bytes::from)),
    };
    (response, flow, changes)
}

/// The headers in `after` that are new or differ from `before`.
fn changed_headers(before: &HeaderMap, after: &HeaderMap) -> HeaderMap {
    let mut changed = HeaderMap::new();
    for name in after.keys() {
        if !after.get_all(name).iter().eq(before.get_all(name).iter()) {
            for value in after.get_all(name) {
                changed.append(name.clone(), value.clone());
            }
        }
    }
    changed
}

fn is_compressed(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != "identity")
}

/// The whole body when it fits in `cap` bytes. Otherwise, or on a read error, a body that replays
/// the bytes already read and then the rest, so the client still gets the upstream body.
async fn read_capped(response: reqwest::Response, cap: usize) -> Result<Bytes, reqwest::Body> {
    let mut chunks = response.bytes_stream();
    let mut read = BytesMut::new();
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                read.extend_from_slice(&chunk);
                if read.len() > cap {
                    let head = stream::once(async move { Ok(read.freeze()) });
                    return Err(reqwest::Body::wrap_stream(head.chain(chunks)));
                }
            }
            Err(error) => {
                let replay = stream::iter([Ok(read.freeze()), Err(error)]);
                return Err(reqwest::Body::wrap_stream(replay));
            }
        }
    }
    Ok(read.freeze())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;
    use crate::ext::{Api, Extension};

    fn upstream(
        status: u16,
        headers: &[(&str, &str)],
        body: impl Into<reqwest::Body>,
    ) -> reqwest::Response {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        rebuilt_response(
            StatusCode::from_u16(status).unwrap(),
            reqwest::Version::HTTP_11,
            map,
            body,
        )
    }

    fn target() -> ResponseTarget<'static> {
        ResponseTarget {
            profile: "p",
            model: "p/m",
            api: Api::Chat,
            client_path: "/v1/chat/completions",
            copilot: false,
            last_attempt: true,
        }
    }

    /// Reads every error body and renames `error.code` to `renamed`.
    struct Renamer;

    impl Extension for Renamer {
        fn name(&self) -> &'static str {
            "renamer"
        }

        fn wants_error_body(&self, _ctx: &ResponseCtx<'_>) -> bool {
            true
        }

        fn on_response(&self, ctx: &mut ResponseCtx<'_>) -> ResponseFlow {
            ctx.headers_mut()
                .append("vary", HeaderValue::from_static("b"));
            if let Some(body) = ctx.error_body_mut() {
                body["error"]["code"] = json!("renamed");
            }
            ResponseFlow::Continue
        }
    }

    fn renamer() -> ExtensionSet {
        ExtensionSet::new(vec![Arc::new(Renamer)])
    }

    async fn delivered(response: reqwest::Response) -> Response {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap();
        let mut out = Response::new(Body::from(body));
        *out.status_mut() = status;
        *out.headers_mut() = headers;
        out
    }

    #[tokio::test]
    async fn a_changed_error_body_reaches_the_client_and_the_hub_keeps_the_original() {
        let body = json!({"error": {"code": "x"}}).to_string();
        let reply = upstream(
            400,
            &[("vary", "a"), ("content-length", "22")],
            body.clone(),
        );
        let mut state = Extensions::new();

        let (hub_reply, flow, changes) =
            response_phase(&renamer(), target(), reply, &mut state).await;

        assert_eq!(flow, ResponseFlow::Continue);
        let client = changes.apply(delivered(hub_reply).await).await;
        assert!(client.headers().get(header::CONTENT_LENGTH).is_none());
        let vary: Vec<_> = client.headers().get_all("vary").iter().collect();
        assert_eq!(vary, ["a", "b"]);
        let raw = to_bytes(client.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&raw).unwrap(),
            json!({"error": {"code": "renamed"}})
        );
    }

    #[tokio::test]
    async fn a_compressed_error_body_is_not_read() {
        let body = json!({"error": {"code": "x"}}).to_string();
        let reply = upstream(400, &[("content-encoding", "gzip")], body.clone());
        let mut state = Extensions::new();

        let (hub_reply, _, changes) = response_phase(&renamer(), target(), reply, &mut state).await;

        let client = changes.apply(delivered(hub_reply).await).await;
        let raw = to_bytes(client.into_body(), usize::MAX).await.unwrap();
        assert_eq!(raw, body.as_bytes());
    }

    #[tokio::test]
    async fn a_success_body_is_not_read_and_unchanged_replies_pass_through() {
        let reply = upstream(200, &[], "data: {}\n\n");
        let mut state = Extensions::new();

        let (hub_reply, flow, changes) =
            response_phase(&ExtensionSet::default(), target(), reply, &mut state).await;

        assert_eq!(flow, ResponseFlow::Continue);
        assert!(changes.is_empty());
        let client = changes.apply(delivered(hub_reply).await).await;
        let raw = to_bytes(client.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&raw[..], b"data: {}\n\n");
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_replayed_whole() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"abcd")),
            Ok(Bytes::from_static(b"efgh")),
        ];
        let reply = upstream(500, &[], reqwest::Body::wrap_stream(stream::iter(chunks)));

        let Err(rest) = read_capped(reply, 5).await else {
            panic!("the body is over the cap");
        };

        let raw = upstream(500, &[], rest).bytes().await.unwrap();
        assert_eq!(&raw[..], b"abcdefgh");
    }

    #[tokio::test]
    async fn a_read_error_replays_the_bytes_read_then_the_error() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"ab")),
            Err(std::io::Error::other("connection reset")),
        ];
        let reply = upstream(500, &[], reqwest::Body::wrap_stream(stream::iter(chunks)));

        let Err(rest) = read_capped(reply, 1_024).await else {
            panic!("the read failed");
        };

        let mut replayed = upstream(500, &[], rest).bytes_stream();
        assert_eq!(&replayed.next().await.unwrap().unwrap()[..], b"ab");
        assert!(replayed.next().await.unwrap().is_err());
    }

    #[tokio::test]
    async fn removed_headers_and_a_new_status_reach_the_client() {
        let changes = ClientChanges {
            status: Some(StatusCode::TOO_MANY_REQUESTS),
            removed_headers: vec![header::HeaderName::from_static("x-old")],
            ..ClientChanges::default()
        };
        let mut delivered = Response::new(Body::from("{}"));
        delivered
            .headers_mut()
            .insert("x-old", HeaderValue::from_static("1"));

        let client = changes.without_body().apply(delivered).await;

        assert_eq!(client.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!client.headers().contains_key("x-old"));
    }

    #[test]
    fn only_new_or_changed_headers_count_as_changed() {
        let mut before = HeaderMap::new();
        before.insert("same", HeaderValue::from_static("1"));
        before.insert("changed", HeaderValue::from_static("1"));
        let mut after = before.clone();
        after.insert("changed", HeaderValue::from_static("2"));
        after.insert("new", HeaderValue::from_static("3"));

        let changed = changed_headers(&before, &after);

        assert_eq!(changed.len(), 2);
        assert_eq!(changed["changed"], "2");
        assert_eq!(changed["new"], "3");
    }
}

//! Runs the extension phases on the request path. Each function changes nothing when no
//! extension acts, and reads or rebuilds a body only when one asks for it.

use axum::body::{Body, to_bytes};
use axum::http::{Extensions, HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use serde_json::Value;

use crate::consts::MAX_TRANSFORM_BUFFER_BYTES;
use crate::ext::{
    AttemptCtx, AttemptTarget, ExtensionSet, Flow, RequestCtx, ResponseCtx, ResponseFlow,
    ResponseTarget,
};
use crate::schemas::app_error::AppError;
use crate::services::upstream_reply::rebuilt_response;

/// The client request after the `request` phase: changed headers, if any, and the body to use.
pub(crate) fn request_phase(
    set: &ExtensionSet,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
    state: &mut Extensions,
) -> Result<(Option<HeaderMap>, Bytes), AppError> {
    let mut ctx = RequestCtx::new(method, path, headers, body, state);
    rejected(set.request(&mut ctx))?;
    Ok(ctx.into_parts())
}

/// One attempt after the `attempt` phase: changed headers, if any, and the body to send.
pub(crate) fn attempt_phase(
    set: &ExtensionSet,
    target: AttemptTarget<'_>,
    headers: &HeaderMap,
    body: Bytes,
    state: &mut Extensions,
) -> Result<(Option<HeaderMap>, Bytes), AppError> {
    let mut ctx = AttemptCtx::new(target, headers, body, state);
    rejected(set.attempt(&mut ctx))?;
    Ok(ctx.into_parts())
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
                let _ = to_bytes(body, MAX_TRANSFORM_BUFFER_BYTES).await;
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
        for (name, value) in &self.set_headers {
            parts.headers.insert(name.clone(), value.clone());
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
        let values: Vec<_> = after.get_all(name).iter().collect();
        let old: Vec<_> = before.get_all(name).iter().collect();
        if values != old {
            for value in values {
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

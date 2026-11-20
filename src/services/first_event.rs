//! Holds back the start of a 2xx upstream reply until its first meaningful event, so the attempt
//! loop can still try the next fallback when the reply is empty or opens with an error event.
//! Only the bytes up to that event are held. The rest streams through untouched.

use std::time::Duration;

use axum::http::header;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::time::Instant;

use crate::services::upstream_reply::rebuilt_response;

/// The most bytes held while no meaningful event has arrived. Past it, the reply passes as is.
const MAX_HELD_BYTES: usize = 64 * 1024;
/// How long a check with no time limit waits for the first event before it lets the reply
/// through, so a slow local model still reaches the client and its keepalive.
pub const CHECK_WINDOW: Duration = Duration::from_secs(30);
/// The longest upstream text in a failure reason.
const MAX_REASON_CHARS: usize = 160;

/// How [`first_event`] treats one reply.
#[derive(Debug, Clone, Copy)]
pub struct FirstEventCheck {
    /// The wait for the first byte, or with `judge`, the first event. Past it the attempt fails
    /// with `timeout`. `None` waits up to [`CHECK_WINDOW`] when judging, else forever.
    pub limit: Option<Duration>,
    /// Fail an empty reply or an opening error event. Off on the last attempt, where the client
    /// should see what the upstream sent.
    pub judge: bool,
}

/// What the bytes held so far say.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Content arrived: deliver the reply.
    Content,
    /// Only comments or pings so far.
    Undecided,
    /// The first event is an error, with its message.
    Error(String),
    /// The stream ended before any content (`[DONE]` first).
    Done,
}

/// The reply with the held bytes put back in front, or the failure reason for the attempt loop:
/// `timeout: ...`, `empty reply: ...`, `error event: ...` or `read error: ...`.
///
/// # Errors
/// The reason text when the attempt failed.
pub async fn first_event(
    response: reqwest::Response,
    check: FirstEventCheck,
) -> Result<reqwest::Response, String> {
    first_event_within(response, check, CHECK_WINDOW).await
}

async fn first_event_within(
    response: reqwest::Response,
    check: FirstEventCheck,
    check_window: Duration,
) -> Result<reqwest::Response, String> {
    let (status, version, headers) = (
        response.status(),
        response.version(),
        response.headers().clone(),
    );
    let sse = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let opaque = headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding != "identity");
    let window = match (check.limit, check.judge) {
        (Some(limit), _) => Some(limit),
        (None, true) => Some(check_window),
        (None, false) => None,
    };
    let ends = window.map(|wait| Instant::now() + wait);
    let mut chunks = response.bytes_stream();
    let mut held = BytesMut::new();
    loop {
        let next = match ends {
            Some(ends) => match tokio::time::timeout_at(ends, chunks.next()).await {
                Ok(next) => next,
                Err(_elapsed) if check.limit.is_none() => break,
                Err(_elapsed) if held.is_empty() => {
                    return Err("timeout: no first byte".to_owned());
                }
                Err(_elapsed) => return Err("timeout: no first event".to_owned()),
            },
            None => chunks.next().await,
        };
        match next {
            None => {
                if !check.judge {
                    break;
                }
                return match verdict(&held, sse, opaque, true) {
                    Verdict::Content => break,
                    Verdict::Error(message) => Err(format!("error event: {message}")),
                    Verdict::Undecided | Verdict::Done => {
                        Err("empty reply: the upstream sent no content".to_owned())
                    }
                };
            }
            Some(Err(error)) => return Err(format!("read error: {error}")),
            Some(Ok(chunk)) => held.extend_from_slice(&chunk),
        }
        if !check.judge && !held.is_empty() {
            break;
        }
        match verdict(&held, sse, opaque, false) {
            Verdict::Undecided if held.len() <= MAX_HELD_BYTES => {}
            Verdict::Content | Verdict::Undecided => break,
            Verdict::Error(message) => return Err(format!("error event: {message}")),
            Verdict::Done => {
                return Err("empty reply: the stream ended before any content".to_owned());
            }
        }
    }
    let replay = futures_util::stream::iter(
        (!held.is_empty()).then(|| Ok::<Bytes, reqwest::Error>(held.freeze())),
    );
    let body = reqwest::Body::wrap_stream(replay.chain(chunks));
    Ok(rebuilt_response(status, version, headers, body))
}

/// Reads the held bytes. A plain or compressed body counts as content at its first non-space
/// byte. An SSE body is read event by event. `ended` reads a last event with no blank line.
fn verdict(held: &[u8], sse: bool, opaque: bool, ended: bool) -> Verdict {
    let start = held.trim_ascii_start();
    let sse = sse || start.starts_with(b"data:") || start.starts_with(b"event:");
    if opaque || !sse {
        return if held.iter().any(|byte| !byte.is_ascii_whitespace()) {
            Verdict::Content
        } else {
            Verdict::Undecided
        };
    }
    let text = String::from_utf8_lossy(held);
    let text = text.replace("\r\n", "\n");
    let mut events: Vec<&str> = text.split("\n\n").collect();
    if !ended {
        events.pop();
    }
    for event in events {
        match event_verdict(event) {
            Verdict::Undecided => {}
            decided => return decided,
        }
    }
    Verdict::Undecided
}

/// One SSE event: comments, pings and blank events are undecided.
fn event_verdict(event: &str) -> Verdict {
    let mut name: Option<&str> = None;
    let mut data = String::new();
    for line in event.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            name = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    let data = data.trim();
    if name == Some("error") {
        return Verdict::Error(error_text(data));
    }
    if data.is_empty() || name == Some("ping") {
        return Verdict::Undecided;
    }
    if data == "[DONE]" {
        return Verdict::Done;
    }
    let Ok(json) = serde_json::from_str::<Value>(data) else {
        return Verdict::Content;
    };
    match json.get("type").and_then(Value::as_str) {
        Some("ping") => return Verdict::Undecided,
        Some("error") => return Verdict::Error(error_text(data)),
        _ => {}
    }
    if json.get("error").is_some_and(|error| !error.is_null()) {
        return Verdict::Error(error_text(data));
    }
    Verdict::Content
}

/// The message of an error event, or its raw data, shortened.
fn error_text(data: &str) -> String {
    let message = serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|json| crate::ext::error_message_of(&json).map(str::to_owned));
    let text = message.unwrap_or_else(|| data.to_owned());
    let text = if text.is_empty() {
        "no message".to_owned()
    } else {
        text
    };
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use futures_util::stream;

    use super::*;

    const JUDGE: FirstEventCheck = FirstEventCheck {
        limit: None,
        judge: true,
    };

    fn reply(
        content_type: Option<&'static str>,
        body: impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + Sync + 'static,
    ) -> reqwest::Response {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(value));
        }
        rebuilt_response(
            StatusCode::OK,
            reqwest::Version::HTTP_11,
            headers,
            reqwest::Body::wrap_stream(body),
        )
    }

    fn sse(chunks: &[&'static str]) -> reqwest::Response {
        let chunks: Vec<Result<Bytes, std::io::Error>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from_static(c.as_bytes())))
            .collect();
        reply(Some("text/event-stream"), stream::iter(chunks))
    }

    #[tokio::test]
    async fn a_content_event_passes_with_every_byte_kept() {
        let upstream = sse(&[
            ": ping\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "data: {\"choices\":[]}",
            "\n\ndata: [DONE]\n\n",
        ]);

        let delivered = first_event(upstream, JUDGE).await.unwrap();

        let body = delivered.bytes().await.unwrap();
        assert_eq!(
            &body[..],
            b": ping\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\ndata: {\"choices\":[]}\n\ndata: [DONE]\n\n"
        );
    }

    #[tokio::test]
    async fn an_empty_body_or_a_comment_only_stream_is_an_empty_reply() {
        for upstream in [
            sse(&[]),
            sse(&[": keepalive\n\n"]),
            reply(None, stream::empty()),
        ] {
            let reason = first_event(upstream, JUDGE).await.unwrap_err();
            assert!(reason.starts_with("empty reply"), "{reason}");
        }
        let reason = first_event(sse(&["data: [DONE]\n\n"]), JUDGE)
            .await
            .unwrap_err();
        assert!(reason.starts_with("empty reply"), "{reason}");
    }

    #[tokio::test]
    async fn an_opening_error_event_fails_the_attempt() {
        for (chunks, message) in [
            (
                vec![
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
                ],
                "Overloaded",
            ),
            (
                vec!["data: {\"error\":{\"message\":\"upstream failed\"}}\n\n"],
                "upstream failed",
            ),
            (vec!["event: error\ndata: plain text\n\n"], "plain text"),
            (vec!["event: error\n\n"], "no message"),
            (
                vec!["data: {\"type\":\"error\",\"message\":\"boom\"}"],
                "boom",
            ),
        ] {
            let reason = first_event(sse(&chunks), JUDGE).await.unwrap_err();
            assert_eq!(reason, format!("error event: {message}"));
        }
    }

    #[tokio::test]
    async fn an_error_after_content_is_not_judged() {
        let upstream = sse(&["data: {\"choices\":[]}\n\nevent: error\ndata: {}\n\n"]);
        assert!(first_event(upstream, JUDGE).await.is_ok());
        let crlf = sse(&["data: hello\r\n\r\n"]);
        assert!(first_event(crlf, JUDGE).await.is_ok());
    }

    #[tokio::test]
    async fn plain_and_compressed_bodies_pass_at_their_first_byte() {
        let json = reply(
            Some("application/json"),
            stream::iter([Ok(Bytes::from_static(b"  ")), Ok(Bytes::from_static(b"{}"))]),
        );
        assert_eq!(
            &first_event(json, JUDGE)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()[..],
            b"  {}"
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        let gzip = rebuilt_response(
            StatusCode::OK,
            reqwest::Version::HTTP_11,
            headers,
            Bytes::from_static(b"\x1f\x8b"),
        );
        assert!(first_event(gzip, JUDGE).await.is_ok());
    }

    #[tokio::test]
    async fn a_silent_reply_hits_the_limit() {
        let check = FirstEventCheck {
            limit: Some(Duration::from_millis(30)),
            judge: true,
        };
        let silent = reply(Some("text/event-stream"), stream::pending());
        assert_eq!(
            first_event(silent, check).await.unwrap_err(),
            "timeout: no first byte"
        );

        let pinged = reply(
            Some("text/event-stream"),
            stream::iter([Ok(Bytes::from_static(b": ping\n\n"))]).chain(stream::pending()),
        );
        assert_eq!(
            first_event(pinged, check).await.unwrap_err(),
            "timeout: no first event"
        );
    }

    #[tokio::test]
    async fn without_a_limit_the_check_gives_up_after_its_window() {
        let pinged = reply(
            Some("text/event-stream"),
            stream::iter([Ok(Bytes::from_static(b": ping\n\n"))]).chain(stream::pending()),
        );

        let delivered = first_event_within(pinged, JUDGE, Duration::from_millis(30))
            .await
            .unwrap();

        let mut body = delivered.bytes_stream();
        assert_eq!(&body.next().await.unwrap().unwrap()[..], b": ping\n\n");
    }

    #[tokio::test]
    async fn a_large_undecided_prefix_passes_through() {
        let comment: &'static str =
            Box::leak(format!(": {}\n\n", "x".repeat(MAX_HELD_BYTES)).into_boxed_str());
        let upstream = reply(
            Some("text/event-stream"),
            stream::iter([Ok(Bytes::from_static(comment.as_bytes()))]).chain(stream::pending()),
        );
        assert!(first_event(upstream, JUDGE).await.is_ok());
    }

    #[tokio::test]
    async fn without_judging_only_the_first_chunk_is_awaited() {
        let check = FirstEventCheck {
            limit: Some(Duration::from_secs(5)),
            judge: false,
        };
        let empty = first_event(sse(&[]), check).await.unwrap();
        assert!(empty.bytes().await.unwrap().is_empty());
        let error = first_event(sse(&["event: error\ndata: {}\n\n"]), check)
            .await
            .unwrap();
        assert_eq!(
            &error.bytes().await.unwrap()[..],
            b"event: error\ndata: {}\n\n"
        );
        let unlimited = FirstEventCheck {
            limit: None,
            judge: false,
        };
        assert!(first_event(sse(&["data: x\n\n"]), unlimited).await.is_ok());
    }

    #[tokio::test]
    async fn a_read_error_fails_the_attempt() {
        let broken = reply(
            Some("text/event-stream"),
            stream::iter([Err(std::io::Error::other("reset"))]),
        );
        assert!(
            first_event(broken, JUDGE)
                .await
                .unwrap_err()
                .starts_with("read error")
        );
    }
}

//! Copilot names a Responses context overflow `model_max_prompt_tokens_exceeded`. `OpenAI`
//! clients such as Codex compact the conversation only on `context_length_exceeded`, and end the
//! turn on any other code. The hub renames the code and keeps the status and the rest of the body.

use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::BytesMut;
use futures_util::{StreamExt, stream};
use serde_json::Value;

use crate::consts::MAX_TRANSFORM_BUFFER_BYTES;

/// The code Copilot sends when a prompt is over the model's input limit.
const COPILOT_CONTEXT_CODE: &str = "model_max_prompt_tokens_exceeded";
/// The code `OpenAI` and its clients use for the same rejection.
const OPENAI_CONTEXT_CODE: &str = "context_length_exceeded";

/// `response` with Copilot's context code renamed. Any other response passes through unchanged.
/// Call it after the stats observer wraps the body, so stats and limit learning see the original.
pub async fn rename_context_code(response: Response) -> Response {
    if response.status() != StatusCode::BAD_REQUEST
        || response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|encoding| encoding != "identity")
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let raw = match read_capped(body, MAX_TRANSFORM_BUFFER_BYTES).await {
        Ok(raw) => raw,
        Err(replay) => return Response::from_parts(parts, replay),
    };
    let Some(renamed) = renamed_body(&raw) else {
        return Response::from_parts(parts, Body::from(raw));
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(renamed))
}

/// The body with `error.code` renamed, or `None` when it is not Copilot's context rejection.
fn renamed_body(raw: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(raw).ok()?;
    let code = value.get_mut("error")?.get_mut("code")?;
    if code.as_str() != Some(COPILOT_CONTEXT_CODE) {
        return None;
    }
    *code = Value::from(OPENAI_CONTEXT_CODE);
    serde_json::to_vec(&value).ok()
}

/// The whole body when it fits in `cap` bytes. Otherwise, or on a read error, a body that replays
/// the bytes already read and then the rest, so the client still gets the upstream body.
async fn read_capped(body: Body, cap: usize) -> Result<Bytes, Body> {
    let mut chunks = body.into_data_stream();
    let mut read = BytesMut::new();
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                read.extend_from_slice(&chunk);
                if read.len() > cap {
                    let head = stream::once(async move { Ok(read.freeze()) });
                    return Err(Body::from_stream(head.chain(chunks)));
                }
            }
            Err(error) => {
                return Err(Body::from_stream(stream::iter([
                    Ok(read.freeze()),
                    Err(error),
                ])));
            }
        }
    }
    Ok(read.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bad_request(body: &Value) -> Response {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, body.to_string().len())
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body_json(response: Response) -> Value {
        let raw = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&raw).unwrap()
    }

    #[tokio::test]
    async fn copilot_context_code_is_renamed_and_the_status_and_message_stay() {
        let upstream = json!({"error": {
            "code": COPILOT_CONTEXT_CODE,
            "message": "prompt is too long: 1200000 tokens > 900000 maximum",
        }});

        let response = rename_context_code(bad_request(&upstream)).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], OPENAI_CONTEXT_CODE);
        assert_eq!(
            body["error"]["message"],
            "prompt is too long: 1200000 tokens > 900000 maximum"
        );
    }

    #[tokio::test]
    async fn other_errors_pass_through_unchanged() {
        for upstream in [
            json!({"error": {"code": "invalid_request_error", "message": "bad tool schema"}}),
            json!({"error": "plain string"}),
            json!({"message": "no error object"}),
        ] {
            let response = rename_context_code(bad_request(&upstream)).await;
            assert_eq!(body_json(response).await, upstream);
        }
    }

    #[tokio::test]
    async fn a_non_400_is_not_read() {
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(
                r#"{"error":{"code":"model_max_prompt_tokens_exceeded"}}"#,
            ))
            .unwrap();

        let out = rename_context_code(response).await;

        let raw = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            &raw[..],
            br#"{"error":{"code":"model_max_prompt_tokens_exceeded"}}"#
        );
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_replayed_whole() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"abcd")),
            Ok(Bytes::from_static(b"efgh")),
        ];
        let body = Body::from_stream(stream::iter(chunks));

        let Err(replay) = read_capped(body, 5).await else {
            panic!("the body is over the cap");
        };

        let raw = axum::body::to_bytes(replay, usize::MAX).await.unwrap();
        assert_eq!(&raw[..], b"abcdefgh");
    }

    #[tokio::test]
    async fn a_compressed_400_is_not_read() {
        let compressed = b"\x1f\x8b not json";
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(&compressed[..]))
            .unwrap();

        let out = rename_context_code(response).await;

        let raw = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&raw[..], compressed);
    }

    #[tokio::test]
    async fn a_400_over_the_cap_reaches_the_client_whole() {
        let oversized = vec![b' '; MAX_TRANSFORM_BUFFER_BYTES + 1];
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from(oversized.clone()))
            .unwrap();

        let out = rename_context_code(response).await;

        assert_eq!(out.status(), StatusCode::BAD_REQUEST);
        let raw = axum::body::to_bytes(out.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(raw.len(), oversized.len());
    }

    #[tokio::test]
    async fn a_read_error_replays_the_bytes_read_then_the_error() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"ab")),
            Err(std::io::Error::other("connection reset")),
        ];
        let body = Body::from_stream(stream::iter(chunks));

        let Err(replay) = read_capped(body, 1_024).await else {
            panic!("the read failed");
        };

        let mut replayed = replay.into_data_stream();
        assert_eq!(&replayed.next().await.unwrap().unwrap()[..], b"ab");
        assert!(replayed.next().await.unwrap().is_err());
    }
}

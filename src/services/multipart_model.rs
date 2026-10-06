//! Reads and rewrites the `model` field of a `multipart/form-data` body, so
//! audio and file endpoints route and fail over like JSON ones.

use bytes::Bytes;

const MODEL_FIELD_DISPOSITION: &str = "name=\"model\"";
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";
const LINE_BREAK: &[u8] = b"\r\n";

/// The boundary of a `multipart/*` content type, with its original casing.
pub fn boundary(content_type: &str) -> Option<String> {
    let (mime, params) = content_type.split_once(';')?;
    if !mime.trim().to_ascii_lowercase().starts_with("multipart/") {
        return None;
    }
    params.split(';').find_map(|param| {
        let (key, value) = param.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// The value of the `model` field, when the body has one.
pub fn model_field(body: &[u8], boundary: &str) -> Option<String> {
    let (start, end) = model_value_range(body, boundary)?;
    std::str::from_utf8(&body[start..end])
        .ok()
        .map(|value| value.trim().to_string())
}

/// A copy of `body` whose `model` field holds `model`; `None` when there is no
/// `model` field to replace.
pub fn with_model_field(body: &[u8], boundary: &str, model: &str) -> Option<Bytes> {
    let (start, end) = model_value_range(body, boundary)?;
    let mut rewritten = Vec::with_capacity(body.len() - (end - start) + model.len());
    rewritten.extend_from_slice(&body[..start]);
    rewritten.extend_from_slice(model.as_bytes());
    rewritten.extend_from_slice(&body[end..]);
    Some(Bytes::from(rewritten))
}

fn model_value_range(body: &[u8], boundary: &str) -> Option<(usize, usize)> {
    let delimiter = format!("--{boundary}").into_bytes();
    let delimiters = positions(body, &delimiter);
    delimiters.windows(2).find_map(|pair| {
        let part_start = pair[0] + delimiter.len();
        let part_end = pair[1];
        let part = &body[part_start..part_end];
        let header_end = find(part, HEADER_TERMINATOR, 0)?;
        if !is_model_part(&part[..header_end]) {
            return None;
        }
        let value_start = part_start + header_end + HEADER_TERMINATOR.len();
        let value_end = part_end.checked_sub(LINE_BREAK.len())?;
        (value_start <= value_end).then_some((value_start, value_end))
    })
}

fn is_model_part(headers: &[u8]) -> bool {
    let Ok(headers) = std::str::from_utf8(headers) else {
        return false;
    };
    headers
        .lines()
        .filter(|line| {
            line.trim_start()
                .to_ascii_lowercase()
                .starts_with("content-disposition:")
        })
        .flat_map(|line| line.split(';'))
        .any(|param| param.trim() == MODEL_FIELD_DISPOSITION)
}

fn positions(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(index) = find(haystack, needle, from) {
        found.push(index);
        from = index + needle.len();
    }
    found
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDARY: &str = "Xy-Boundary42";

    fn form(model_disposition: &str, model: &str) -> Vec<u8> {
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; {model_disposition}\r\n\r\n{model}\r\n\
             --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.m4a\"\r\n\
             Content-Type: audio/mp4\r\n\r\nAUDIO\r\nBYTES\r\n--{BOUNDARY}--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn boundary_quoted_and_mixed_case_keeps_original_casing() {
        let content_type = "Multipart/Form-Data; Boundary=\"Xy-Boundary42\"";
        assert_eq!(boundary(content_type).as_deref(), Some(BOUNDARY));
    }

    #[test]
    fn boundary_not_multipart_returns_none() {
        assert_eq!(boundary("application/json; boundary=abc"), None);
        assert_eq!(boundary("multipart/form-data"), None);
    }

    #[test]
    fn model_field_present_returns_value() {
        let body = form("name=\"model\"", "whisper/large-v3");
        assert_eq!(
            model_field(&body, BOUNDARY).as_deref(),
            Some("whisper/large-v3")
        );
    }

    #[test]
    fn model_field_only_filename_matches_returns_none() {
        let body = form("name=\"other\"; filename=\"model\"", "x");
        assert_eq!(model_field(&body, BOUNDARY), None);
    }

    #[test]
    fn with_model_field_replaces_value_and_keeps_other_parts() {
        let body = form("name=\"model\"", "whisper/large-v3");
        let rewritten = with_model_field(&body, BOUNDARY, "large-v3").unwrap();
        assert_eq!(rewritten, Bytes::from(form("name=\"model\"", "large-v3")));
    }

    #[test]
    fn with_model_field_no_model_part_returns_none() {
        let body = form("name=\"prompt\"", "hello");
        assert_eq!(with_model_field(&body, BOUNDARY, "m"), None);
    }
}

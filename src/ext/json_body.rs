use bytes::Bytes;
use serde_json::Value;

/// A request body that is parsed as JSON only when an extension reads it, and serialized again
/// only when one changed it.
#[derive(Debug)]
pub(crate) struct JsonBody {
    raw: Bytes,
    parsed: Parsed,
    changed: bool,
}

#[derive(Debug)]
enum Parsed {
    NotYet,
    Json(Value),
    NotJson,
}

impl JsonBody {
    pub(crate) fn new(raw: Bytes) -> JsonBody {
        JsonBody {
            raw,
            parsed: Parsed::NotYet,
            changed: false,
        }
    }

    /// The body as JSON, or `None` when it is empty or not JSON.
    pub(crate) fn get(&mut self) -> Option<&Value> {
        self.parse();
        match &self.parsed {
            Parsed::Json(value) => Some(value),
            Parsed::NotYet | Parsed::NotJson => None,
        }
    }

    /// The body as JSON for a change, or `None` when it is empty or not JSON.
    pub(crate) fn get_mut(&mut self) -> Option<&mut Value> {
        self.parse();
        match &mut self.parsed {
            Parsed::Json(value) => {
                self.changed = true;
                Some(value)
            }
            Parsed::NotYet | Parsed::NotJson => None,
        }
    }

    /// The body to send: the original bytes, or the changed JSON.
    pub(crate) fn into_bytes(self) -> Bytes {
        match self.parsed {
            Parsed::Json(value) if self.changed => {
                serde_json::to_vec(&value).map_or(self.raw, Bytes::from)
            }
            _ => self.raw,
        }
    }

    fn parse(&mut self) {
        if matches!(self.parsed, Parsed::NotYet) {
            self.parsed = serde_json::from_slice(&self.raw).map_or(Parsed::NotJson, Parsed::Json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unread_body_keeps_its_bytes() {
        let raw = Bytes::from_static(b"{ \"model\" :  \"a\" }");

        let body = JsonBody::new(raw.clone());

        assert_eq!(body.into_bytes(), raw);
    }

    #[test]
    fn a_read_but_unchanged_body_keeps_its_bytes() {
        let raw = Bytes::from_static(b"{ \"model\" :  \"a\" }");
        let mut body = JsonBody::new(raw.clone());

        assert_eq!(body.get(), Some(&json!({"model": "a"})));
        assert_eq!(body.get(), Some(&json!({"model": "a"})));

        assert_eq!(body.into_bytes(), raw);
    }

    #[test]
    fn a_changed_body_is_serialized_again() {
        let mut body = JsonBody::new(Bytes::from_static(br#"{"model":"a","temperature":1}"#));

        body.get_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("temperature");

        assert_eq!(&body.into_bytes()[..], br#"{"model":"a"}"#);
    }

    #[test]
    fn a_body_that_is_not_json_has_no_value_and_keeps_its_bytes() {
        let raw = Bytes::from_static(b"--boundary\r\nnot json");
        let mut body = JsonBody::new(raw.clone());

        assert!(body.get().is_none());
        assert!(body.get_mut().is_none());

        assert_eq!(body.into_bytes(), raw);
    }
}

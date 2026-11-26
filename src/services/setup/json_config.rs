use serde_json::{Map, Value};

/// Reads, merges and renders a JSON config object, keeping the keys it does not own.
#[derive(Debug)]
pub struct JsonConfig;

impl JsonConfig {
    /// The root object of `existing`; an absent or blank file gives an empty object.
    ///
    /// # Errors
    /// Returns a message naming `label` when the text is not a JSON object.
    pub fn parse_object(existing: Option<&str>, label: &str) -> Result<Map<String, Value>, String> {
        let Some(text) = existing.filter(|text| !text.trim().is_empty()) else {
            return Ok(Map::new());
        };
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) => Err(format!("{label} is not a JSON object")),
            Err(e) => Err(format!("{label} is not valid JSON: {e}")),
        }
    }

    /// The object under `key`, made an empty object when it is missing or another type.
    ///
    /// # Errors
    /// Returns a message when the entry cannot be edited as an object.
    pub fn object_entry<'a>(
        map: &'a mut Map<String, Value>,
        key: &str,
    ) -> Result<&'a mut Map<String, Value>, String> {
        if !matches!(map.get(key), Some(Value::Object(_))) {
            map.insert(key.to_string(), Value::Object(Map::new()));
        }
        map.get_mut(key)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("cannot edit `{key}` as an object"))
    }

    /// Copies `source` into `target`. Objects merge key by key; every other value replaces.
    pub fn merge(target: &mut Map<String, Value>, source: Map<String, Value>) {
        for (key, value) in source {
            if let Value::Object(incoming) = value {
                if let Some(Value::Object(existing)) = target.get_mut(&key) {
                    Self::merge(existing, incoming);
                } else {
                    target.insert(key, Value::Object(incoming));
                }
            } else {
                target.insert(key, value);
            }
        }
    }

    /// Pretty JSON with a trailing newline.
    ///
    /// # Errors
    /// Returns a message when the value cannot be serialized.
    pub fn render(map: Map<String, Value>) -> Result<String, String> {
        serde_json::to_string_pretty(&Value::Object(map))
            .map(|text| format!("{text}\n"))
            .map_err(|e| format!("cannot render the config: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: &Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn missing_or_blank_text_is_an_empty_object() {
        assert!(JsonConfig::parse_object(None, "f").unwrap().is_empty());
        assert!(
            JsonConfig::parse_object(Some(" \n"), "f")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn text_that_is_not_an_object_is_an_error() {
        assert_eq!(
            JsonConfig::parse_object(Some("[1]"), "f.json"),
            Err("f.json is not a JSON object".to_string())
        );
        assert!(
            JsonConfig::parse_object(Some("{"), "f.json")
                .unwrap_err()
                .starts_with("f.json is not valid JSON")
        );
    }

    #[test]
    fn object_entry_replaces_a_non_object() {
        let mut map = object(&json!({"a": 1, "b": {"keep": true}}));

        JsonConfig::object_entry(&mut map, "a")
            .unwrap()
            .insert("x".into(), json!(1));
        JsonConfig::object_entry(&mut map, "b")
            .unwrap()
            .insert("y".into(), json!(2));

        assert_eq!(
            Value::Object(map),
            json!({"a": {"x": 1}, "b": {"keep": true, "y": 2}})
        );
    }

    #[test]
    fn merge_recurses_into_objects_and_replaces_the_rest() {
        let mut target = object(&json!({"env": {"A": "1", "B": "2"}, "list": [1], "k": "old"}));

        JsonConfig::merge(
            &mut target,
            object(&json!({"env": {"B": "3"}, "list": [2], "k": {"n": 1}, "new": 1})),
        );

        assert_eq!(
            Value::Object(target),
            json!({"env": {"A": "1", "B": "3"}, "list": [2], "k": {"n": 1}, "new": 1})
        );
    }

    #[test]
    fn render_ends_with_a_newline() {
        let text = JsonConfig::render(object(&json!({"a": 1}))).unwrap();

        assert_eq!(text, "{\n  \"a\": 1\n}\n");
    }
}

//! Fills the sampling defaults and the thinking switch into a chat request body.

use serde_json::{Map, Value};

use super::super::mistralrs_flags::MistralrsOptions;

/// Sampling values and thinking switch that a request gets when it does not set them.
#[derive(Debug, Clone, PartialEq)]
pub struct SamplingDefaults {
    /// `--temp`.
    pub temp: f64,
    /// `--top-p`.
    pub top_p: f64,
    /// `--top-k`.
    pub top_k: u64,
    /// `--min-p`.
    pub min_p: f64,
    /// `--reasoning`: sent as `enable_thinking` when set.
    pub enable_thinking: Option<bool>,
}

impl SamplingDefaults {
    /// The defaults of `options`.
    pub fn from_options(options: &MistralrsOptions) -> Self {
        Self {
            temp: options.temp,
            top_p: options.top_p,
            top_k: options.top_k,
            min_p: options.min_p,
            enable_thinking: options.reasoning,
        }
    }
}

/// Returns `body` with each missing sampling field set from `defaults`. A body that is not a
/// JSON object comes back unchanged.
pub fn fill_request_body(body: &[u8], defaults: &SamplingDefaults) -> Vec<u8> {
    let Ok(Value::Object(mut request)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    insert_missing(&mut request, "temperature", defaults.temp);
    insert_missing(&mut request, "top_p", defaults.top_p);
    insert_missing(&mut request, "top_k", defaults.top_k);
    insert_missing(&mut request, "min_p", defaults.min_p);
    if let Some(on) = defaults.enable_thinking {
        fill_thinking(&mut request, on);
    }
    serde_json::to_vec(&Value::Object(request)).unwrap_or_else(|_| body.to_vec())
}

fn insert_missing(request: &mut Map<String, Value>, key: &str, value: impl Into<Value>) {
    request.entry(key).or_insert_with(|| value.into());
}

/// Sets `enable_thinking` unless the request sets it at the top level or in `chat_template_kwargs`.
fn fill_thinking(request: &mut Map<String, Value>, on: bool) {
    if request.contains_key("enable_thinking") {
        return;
    }
    let kwargs_set = request
        .get("chat_template_kwargs")
        .and_then(Value::as_object)
        .is_some_and(|kwargs| kwargs.contains_key("enable_thinking"));
    if !kwargs_set {
        request.insert("enable_thinking".to_owned(), Value::Bool(on));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(reasoning: Option<bool>) -> SamplingDefaults {
        SamplingDefaults {
            temp: 1.0,
            top_p: 0.95,
            top_k: 40,
            min_p: 0.01,
            enable_thinking: reasoning,
        }
    }

    fn filled(body: &str, reasoning: Option<bool>) -> Value {
        let out = fill_request_body(body.as_bytes(), &defaults(reasoning));
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn missing_sampling_fields_get_the_defaults() {
        let body = filled(r#"{"model":"m","messages":[]}"#, None);
        assert_eq!(body["temperature"], 1.0);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["min_p"], 0.01);
        assert_eq!(body["model"], "m");
        assert!(body.get("enable_thinking").is_none());
    }

    #[test]
    fn a_request_field_wins_over_the_default() {
        let body = filled(r#"{"temperature":0.2,"top_k":5}"#, None);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_k"], 5);
        assert_eq!(body["top_p"], 0.95);
    }

    #[test]
    fn reasoning_off_sets_thinking_off_unless_the_request_sets_it() {
        assert_eq!(filled("{}", Some(false))["enable_thinking"], false);
        assert_eq!(
            filled(r#"{"enable_thinking":true}"#, Some(false))["enable_thinking"],
            true
        );
        let kwargs = filled(
            r#"{"chat_template_kwargs":{"enable_thinking":true}}"#,
            Some(false),
        );
        assert!(kwargs.get("enable_thinking").is_none());
    }

    #[test]
    fn a_body_that_is_not_an_object_comes_back_unchanged() {
        assert_eq!(fill_request_body(b"not json", &defaults(None)), b"not json");
        assert_eq!(fill_request_body(b"[1,2]", &defaults(None)), b"[1,2]");
    }

    #[test]
    fn defaults_follow_the_mapped_options() {
        let options = mistralrs_flags_defaults();
        let mapped = SamplingDefaults::from_options(&options);
        assert_eq!(mapped, defaults(None));
    }

    fn mistralrs_flags_defaults() -> MistralrsOptions {
        crate::services::local_serving::mistralrs_flags::parse("m", 8, &[]).unwrap()
    }
}

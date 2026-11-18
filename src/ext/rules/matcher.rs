use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::ext::Api;
use crate::ext::rules::rule_spec::{MatchSpec, OneOrMany, PhaseName, StatusSpec};
use crate::utils::glob::glob_matches;

/// The compiled `match` of one rule. Every set condition must hold.
#[derive(Debug, Default)]
pub(crate) struct Matcher {
    paths: Vec<String>,
    methods: Vec<Method>,
    apis: Vec<Api>,
    models: Vec<String>,
    profiles: Vec<String>,
    headers: Vec<(HeaderName, String)>,
    headers_absent: Vec<HeaderName>,
    stream: Option<bool>,
    statuses: Vec<StatusMatch>,
    error_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusMatch {
    Code(u16),
    Class(u16),
}

/// What a request looks like to a matcher. Unknown values are `None` and fail their condition.
#[derive(Default, Clone, Copy)]
pub(crate) struct Facts<'a> {
    pub path: Option<&'a str>,
    pub method: Option<&'a Method>,
    pub api: Option<Api>,
    pub model: Option<&'a str>,
    pub profile: Option<&'a str>,
    pub headers: Option<&'a HeaderMap>,
    pub stream: Option<bool>,
    pub status: Option<u16>,
    pub error_code: Option<&'a str>,
}

impl Matcher {
    /// Compiles `spec` for `phase`. A condition the phase cannot check is an error.
    pub fn compile(spec: MatchSpec, phase: PhaseName) -> Result<Matcher, String> {
        check_keys(&spec, phase)?;
        Ok(Matcher {
            paths: list(spec.path),
            methods: list(spec.method)
                .iter()
                .map(|m| {
                    Method::from_bytes(m.to_ascii_uppercase().as_bytes())
                        .map_err(|_| format!("`{m}` is not an HTTP method"))
                })
                .collect::<Result<_, _>>()?,
            apis: list(spec.api)
                .iter()
                .map(|name| parse_api(name))
                .collect::<Result<_, _>>()?,
            models: list(spec.model),
            profiles: list(spec.profile),
            headers: spec
                .header
                .unwrap_or_default()
                .into_iter()
                .map(|(name, value)| Ok((header_name(&name)?, value)))
                .collect::<Result<_, String>>()?,
            headers_absent: list(spec.header_absent)
                .iter()
                .map(|name| header_name(name))
                .collect::<Result<_, _>>()?,
            stream: spec.stream,
            statuses: list(spec.status)
                .into_iter()
                .map(parse_status)
                .collect::<Result<_, _>>()?,
            error_codes: list(spec.error_code),
        })
    }

    /// True when the rule needs the body of an error reply to decide.
    pub fn needs_error_body(&self) -> bool {
        !self.error_codes.is_empty()
    }

    /// True when the request model or stream flag is part of the match.
    pub fn needs_request_body(&self) -> bool {
        !self.models.is_empty() || self.stream.is_some()
    }

    /// True when every condition holds. `error_code` is skipped when `skip_error_code` is set,
    /// for the check made before the error body is read.
    pub fn matches(&self, facts: &Facts<'_>, skip_error_code: bool) -> bool {
        let path = facts.path.map(|p| p.split('?').next().unwrap_or(p));
        any_or_empty(&self.paths, |glob| {
            path.is_some_and(|p| glob_matches(glob, p))
        }) && any_or_empty(&self.methods, |m| facts.method == Some(m))
            && any_or_empty(&self.apis, |a| facts.api == Some(*a))
            && any_or_empty(&self.models, |glob| {
                facts.model.is_some_and(|m| glob_matches(glob, m))
            })
            && any_or_empty(&self.profiles, |p| facts.profile == Some(p.as_str()))
            && self.headers.iter().all(|(name, value)| {
                facts
                    .headers
                    .and_then(|h| h.get(name))
                    .is_some_and(|actual| actual.as_bytes() == value.as_bytes())
            })
            && self
                .headers_absent
                .iter()
                .all(|name| facts.headers.is_some_and(|h| !h.contains_key(name)))
            && self
                .stream
                .is_none_or(|wanted| facts.stream == Some(wanted))
            && any_or_empty(&self.statuses, |s| {
                facts.status.is_some_and(|status| s.matches(status))
            })
            && (skip_error_code
                || any_or_empty(&self.error_codes, |c| facts.error_code == Some(c.as_str())))
    }
}

impl StatusMatch {
    fn matches(self, status: u16) -> bool {
        match self {
            StatusMatch::Code(code) => status == code,
            StatusMatch::Class(class) => status / 100 == class,
        }
    }
}

/// Fails when `spec` sets a key that `phase` cannot check.
fn check_keys(spec: &MatchSpec, phase: PhaseName) -> Result<(), String> {
    let allowed: &[&str] = match phase {
        PhaseName::Request => &[
            "path",
            "method",
            "api",
            "model",
            "header",
            "header_absent",
            "stream",
        ],
        PhaseName::Attempt => &[
            "path",
            "api",
            "model",
            "profile",
            "header",
            "header_absent",
            "stream",
        ],
        PhaseName::Response => &[
            "path",
            "api",
            "model",
            "profile",
            "header",
            "header_absent",
            "status",
            "error_code",
        ],
    };
    let used = [
        ("path", spec.path.is_some()),
        ("method", spec.method.is_some()),
        ("api", spec.api.is_some()),
        ("model", spec.model.is_some()),
        ("profile", spec.profile.is_some()),
        ("header", spec.header.is_some()),
        ("header_absent", spec.header_absent.is_some()),
        ("stream", spec.stream.is_some()),
        ("status", spec.status.is_some()),
        ("error_code", spec.error_code.is_some()),
    ];
    match used
        .iter()
        .find(|(key, set)| *set && !allowed.contains(key))
    {
        Some((key, _)) => Err(format!(
            "match key `{key}` is not available in the {} phase",
            phase.as_str()
        )),
        None => Ok(()),
    }
}

fn list<T>(values: Option<OneOrMany<T>>) -> Vec<T> {
    values.map(OneOrMany::into_vec).unwrap_or_default()
}

fn any_or_empty<T>(items: &[T], test: impl Fn(&T) -> bool) -> bool {
    items.is_empty() || items.iter().any(test)
}

fn parse_api(name: &str) -> Result<Api, String> {
    match name {
        "chat" => Ok(Api::Chat),
        "messages" => Ok(Api::Messages),
        "responses" => Ok(Api::Responses),
        "other" => Ok(Api::Other),
        _ => Err(format!(
            "api `{name}` is not one of chat, messages, responses, other"
        )),
    }
}

fn parse_status(spec: StatusSpec) -> Result<StatusMatch, String> {
    match spec {
        StatusSpec::Code(code) if (100..=599).contains(&code) => Ok(StatusMatch::Code(code)),
        StatusSpec::Class(class) => match class.as_bytes() {
            [digit @ b'1'..=b'5', b'x' | b'X', b'x' | b'X'] => {
                Ok(StatusMatch::Class(u16::from(digit - b'0')))
            }
            _ => Err(format!(
                "status `{class}` is not a code or a class such as 5xx"
            )),
        },
        StatusSpec::Code(code) => Err(format!("status {code} is not an HTTP status")),
    }
}

pub(crate) fn header_name(name: &str) -> Result<HeaderName, String> {
    HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes())
        .map_err(|_| format!("`{name}` is not a valid header name"))
}

pub(crate) fn header_value(value: &str) -> Result<HeaderValue, String> {
    HeaderValue::from_str(value).map_err(|_| format!("`{value}` is not a valid header value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> MatchSpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn an_empty_match_matches_everything() {
        let matcher = Matcher::compile(MatchSpec::default(), PhaseName::Request).unwrap();

        assert!(matcher.matches(&Facts::default(), false));
        assert!(!matcher.needs_error_body());
        assert!(!matcher.needs_request_body());
    }

    #[test]
    fn every_request_condition_must_hold() {
        let matcher = Matcher::compile(
            spec("{path: /v1/chat/*, method: post, api: [chat, responses], model: 'copilot/*', header: {x-team: a}, header_absent: x-skip, stream: true}"),
            PhaseName::Request,
        )
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-team", HeaderValue::from_static("a"));
        let facts = Facts {
            path: Some("/v1/chat/completions?x=1"),
            method: Some(&Method::POST),
            api: Some(Api::Chat),
            model: Some("copilot/gpt-5"),
            headers: Some(&headers),
            stream: Some(true),
            ..Facts::default()
        };

        assert!(matcher.needs_request_body());
        assert!(matcher.matches(&facts, false));
        assert!(!matcher.matches(
            &Facts {
                model: Some("groq/x"),
                ..facts
            },
            false
        ));
        let mut skipped = headers.clone();
        skipped.insert("x-skip", HeaderValue::from_static("1"));
        assert!(!matcher.matches(
            &Facts {
                headers: Some(&skipped),
                ..Facts {
                    path: Some("/v1/chat/completions"),
                    method: Some(&Method::POST),
                    api: Some(Api::Chat),
                    model: Some("copilot/gpt-5"),
                    stream: Some(true),
                    ..Facts::default()
                }
            },
            false
        ));
        assert!(!matcher.matches(
            &Facts {
                method: Some(&Method::GET),
                path: Some("/v1/chat/completions"),
                api: Some(Api::Chat),
                model: Some("copilot/gpt-5"),
                headers: Some(&headers),
                stream: Some(true),
                ..Facts::default()
            },
            false
        ));
    }

    #[test]
    fn statuses_match_codes_and_classes_and_error_codes_can_be_skipped() {
        let matcher = Matcher::compile(
            spec("{profile: copilot, status: [400, 5xx], error_code: rate_limit}"),
            PhaseName::Response,
        )
        .unwrap();
        let facts = |status, code| Facts {
            profile: Some("copilot"),
            status: Some(status),
            error_code: code,
            ..Facts::default()
        };

        assert!(matcher.needs_error_body());
        assert!(matcher.matches(&facts(400, Some("rate_limit")), false));
        assert!(matcher.matches(&facts(503, Some("rate_limit")), false));
        assert!(!matcher.matches(&facts(429, Some("rate_limit")), false));
        assert!(!matcher.matches(&facts(400, None), false));
        assert!(matcher.matches(&facts(400, None), true));
    }

    #[test]
    fn a_key_the_phase_cannot_check_is_an_error() {
        for (yaml, phase, key) in [
            ("{profile: copilot}", PhaseName::Request, "profile"),
            ("{status: 400}", PhaseName::Attempt, "status"),
            ("{stream: true}", PhaseName::Response, "stream"),
            ("{method: GET}", PhaseName::Response, "method"),
        ] {
            let error = Matcher::compile(spec(yaml), phase).unwrap_err();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn invalid_values_are_errors() {
        for (yaml, phase) in [
            ("{api: graphql}", PhaseName::Request),
            ("{method: 'BAD METHOD'}", PhaseName::Request),
            ("{header: {'bad header': x}}", PhaseName::Request),
            ("{header_absent: 'bad header'}", PhaseName::Request),
            ("{status: 42}", PhaseName::Response),
            ("{status: 6xx}", PhaseName::Response),
        ] {
            assert!(Matcher::compile(spec(yaml), phase).is_err(), "{yaml}");
        }
    }

    #[test]
    fn missing_facts_fail_their_conditions() {
        let matcher = Matcher::compile(
            spec("{header: {x-a: '1'}, header_absent: x-b}"),
            PhaseName::Attempt,
        )
        .unwrap();

        assert!(!matcher.matches(&Facts::default(), false));
    }

    #[test]
    fn header_values_must_be_valid() {
        assert!(header_value("ok").is_ok());
        assert!(header_value("bad\nvalue").is_err());
    }
}

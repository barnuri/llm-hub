use std::collections::BTreeSet;

use axum::http::{HeaderMap, HeaderValue};

use crate::schemas::app_error::AppError;
use crate::schemas::smart_auto_policy::SmartAutoPolicy;
use crate::schemas::smart_auto_tier::SmartAutoTier;
use crate::schemas::work_role::WorkRole;

/// The Smart Auto policy layer and identity hints a request carries in `x-llm-hub-smart-auto-*`.
/// The caller merges `policy` after the selector layer, so headers only tighten.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SmartAutoHeaders {
    /// The constraints from the tier, include, exclude, minimum and role headers.
    pub policy: SmartAutoPolicy,
    /// The worker identity that scopes affinity and history, e.g. `<run>/<node>/<attempt>`.
    pub worker: Option<String>,
    /// The parent workflow that groups the history of its workers.
    pub parent: Option<String>,
    /// The parent policy handle the worker inherits; it is not proof of authorization.
    pub policy_handle: Option<String>,
}

impl SmartAutoHeaders {
    /// The prefix of every Smart Auto request header; the proxy strips all of them.
    pub const PREFIX: &str = "x-llm-hub-smart-auto-";
    /// The tier name: efficiency, balance, intelligence or fast.
    pub const TIER: &str = "x-llm-hub-smart-auto-tier";
    /// Comma-separated model globs; the header adds one allow-list.
    pub const INCLUDE: &str = "x-llm-hub-smart-auto-include";
    /// Comma-separated model globs that are never eligible.
    pub const EXCLUDE: &str = "x-llm-hub-smart-auto-exclude";
    /// The minimum total context window, e.g. `1m` or `200k`.
    pub const MIN_CONTEXT: &str = "x-llm-hub-smart-auto-min-context";
    /// The minimum prompt capacity, e.g. `1m` or `200k`.
    pub const MIN_INPUT: &str = "x-llm-hub-smart-auto-min-input";
    /// The minimum output capacity, e.g. `32k`.
    pub const MIN_OUTPUT: &str = "x-llm-hub-smart-auto-min-output";
    /// The work role hint, e.g. `implementation`.
    pub const ROLE: &str = "x-llm-hub-smart-auto-role";
    /// The worker identity.
    pub const WORKER: &str = "x-llm-hub-smart-auto-worker";
    /// The parent workflow identity.
    pub const PARENT: &str = "x-llm-hub-smart-auto-parent";
    /// The parent policy handle.
    pub const POLICY: &str = "x-llm-hub-smart-auto-policy";
    /// The reasoning effort a routed request was sent with, on a response.
    pub const EFFORT: &str = "x-llm-hub-smart-auto-effort";
    /// The reason code of a route, on a response.
    pub const REASON: &str = "x-llm-hub-smart-auto-reason";

    const LIST_SEPARATOR: char = ',';
    const MAX_IDENTITY_LENGTH: usize = 256;
    const ROLES: [(&str, WorkRole); 5] = [
        ("planning", WorkRole::Planning),
        ("investigation", WorkRole::Investigation),
        ("implementation", WorkRole::Implementation),
        ("validation", WorkRole::Validation),
        ("conversation", WorkRole::Conversation),
    ];

    /// Reads every `x-llm-hub-smart-auto-*` header. No such header gives an empty layer.
    ///
    /// # Errors
    /// Returns `AppError::BadRequest` for an unknown, repeated, empty or malformed header.
    pub fn from_headers(headers: &HeaderMap) -> Result<SmartAutoHeaders, AppError> {
        let mut parsed = SmartAutoHeaders::default();
        for (name, value) in headers {
            if name.as_str().starts_with(Self::PREFIX) {
                parsed.apply(name.as_str(), value)?;
            }
        }
        Ok(parsed)
    }

    fn apply(&mut self, name: &str, value: &HeaderValue) -> Result<(), AppError> {
        let text = Self::text(name, value)?;
        match name {
            Self::TIER => {
                Self::set_once(name, &mut self.policy.tier, Self::parse_tier(name, text)?)
            }
            Self::INCLUDE => {
                let globs = Self::parse_list(name, text)?;
                match self.policy.include.first_mut() {
                    Some(allow_list) => allow_list.extend(globs),
                    None => self.policy.include.push(globs),
                }
                Ok(())
            }
            Self::EXCLUDE => {
                self.policy.exclude.extend(Self::parse_list(name, text)?);
                Ok(())
            }
            Self::MIN_CONTEXT => Self::set_once(
                name,
                &mut self.policy.min_context_tokens,
                Self::parse_token_count(name, text)?,
            ),
            Self::MIN_INPUT => Self::set_once(
                name,
                &mut self.policy.min_input_tokens,
                Self::parse_token_count(name, text)?,
            ),
            Self::MIN_OUTPUT => Self::set_once(
                name,
                &mut self.policy.min_output_tokens,
                Self::parse_token_count(name, text)?,
            ),
            Self::ROLE => {
                Self::set_once(name, &mut self.policy.role, Self::parse_role(name, text)?)
            }
            Self::WORKER => {
                Self::set_once(name, &mut self.worker, Self::parse_identity(name, text)?)
            }
            Self::PARENT => {
                Self::set_once(name, &mut self.parent, Self::parse_identity(name, text)?)
            }
            Self::POLICY => Self::set_once(
                name,
                &mut self.policy_handle,
                Self::parse_identity(name, text)?,
            ),
            _ => Err(AppError::BadRequest(format!(
                "unknown header {name}: Smart Auto does not support it"
            ))),
        }
    }

    fn text<'value>(name: &str, value: &'value HeaderValue) -> Result<&'value str, AppError> {
        let text = value
            .to_str()
            .map_err(|_| AppError::BadRequest(format!("header {name} must be visible ASCII")))?
            .trim();
        if text.is_empty() {
            return Err(AppError::BadRequest(format!("header {name} is empty")));
        }
        Ok(text)
    }

    fn set_once<T>(name: &str, slot: &mut Option<T>, value: T) -> Result<(), AppError> {
        if slot.is_some() {
            return Err(AppError::BadRequest(format!(
                "header {name} occurs more than once"
            )));
        }
        *slot = Some(value);
        Ok(())
    }

    fn parse_tier(name: &str, text: &str) -> Result<SmartAutoTier, AppError> {
        SmartAutoTier::from_name(text).ok_or_else(|| {
            let names: Vec<&str> = SmartAutoTier::ALL.map(SmartAutoTier::as_str).to_vec();
            AppError::BadRequest(format!(
                "header {name} has unknown tier {text:?}: use one of {}",
                names.join(", ")
            ))
        })
    }

    fn parse_role(name: &str, text: &str) -> Result<WorkRole, AppError> {
        Self::ROLES
            .into_iter()
            .find(|(role_name, _)| role_name.eq_ignore_ascii_case(text))
            .map(|(_, role)| role)
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "header {name} has unknown role {text:?}: use planning, investigation, implementation, validation or conversation"
                ))
            })
    }

    fn parse_token_count(name: &str, text: &str) -> Result<u64, AppError> {
        SmartAutoPolicy::parse_token_count(text)
            .map_err(|error| AppError::BadRequest(format!("header {name}: {}", error.message())))
    }

    fn parse_list(name: &str, text: &str) -> Result<BTreeSet<String>, AppError> {
        let mut globs = BTreeSet::new();
        for glob in text.split(Self::LIST_SEPARATOR).map(str::trim) {
            if glob.is_empty() {
                return Err(AppError::BadRequest(format!(
                    "header {name} has an empty entry in {text:?}"
                )));
            }
            globs.insert(glob.to_string());
        }
        Ok(globs)
    }

    fn parse_identity(name: &str, text: &str) -> Result<String, AppError> {
        if text.len() > Self::MAX_IDENTITY_LENGTH {
            return Err(AppError::BadRequest(format!(
                "header {name} is longer than {} characters",
                Self::MAX_IDENTITY_LENGTH
            )));
        }
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::smart_auto_policy::tests::globs;
    use axum::http::header::HeaderName;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                name.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<SmartAutoHeaders, AppError> {
        SmartAutoHeaders::from_headers(&headers(pairs))
    }

    fn assert_bad_request(pairs: &[(&str, &str)]) {
        let error = parse(pairs).unwrap_err();
        assert!(matches!(error, AppError::BadRequest(_)), "{pairs:?}");
    }

    #[test]
    fn absent_headers_give_an_empty_layer() {
        let parsed = parse(&[
            ("content-type", "application/json"),
            ("x-llm-hub-profiles", "copilot"),
        ])
        .unwrap();
        assert_eq!(parsed, SmartAutoHeaders::default());
        assert_eq!(parsed.policy, SmartAutoPolicy::default());
    }

    #[test]
    fn every_header_parses() {
        let parsed = parse(&[
            (SmartAutoHeaders::TIER, "intelligence"),
            (SmartAutoHeaders::INCLUDE, " copilot/* , llama_swap/*"),
            (SmartAutoHeaders::EXCLUDE, "*fast-preview*"),
            (SmartAutoHeaders::MIN_CONTEXT, "1m"),
            (SmartAutoHeaders::MIN_INPUT, "200k"),
            (SmartAutoHeaders::MIN_OUTPUT, "32000"),
            (SmartAutoHeaders::ROLE, "implementation"),
            (SmartAutoHeaders::WORKER, "run-1/node-2/attempt-1"),
            (SmartAutoHeaders::PARENT, "run-1"),
            (SmartAutoHeaders::POLICY, "policy-abc"),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            SmartAutoHeaders {
                policy: SmartAutoPolicy {
                    tier: Some(SmartAutoTier::Intelligence),
                    include: vec![globs(&["copilot/*", "llama_swap/*"])],
                    exclude: globs(&["*fast-preview*"]),
                    min_context_tokens: Some(1_000_000),
                    min_input_tokens: Some(200_000),
                    min_output_tokens: Some(32_000),
                    role: Some(WorkRole::Implementation),
                },
                worker: Some("run-1/node-2/attempt-1".to_string()),
                parent: Some("run-1".to_string()),
                policy_handle: Some("policy-abc".to_string()),
            }
        );
    }

    #[test]
    fn header_names_match_case_insensitively() {
        let parsed = parse(&[("X-LLM-Hub-Smart-Auto-Tier", "Fast")]).unwrap();
        assert_eq!(parsed.policy.tier, Some(SmartAutoTier::Fast));
    }

    #[test]
    fn every_tier_and_role_name_matches_its_serde_name() {
        for tier in SmartAutoTier::ALL {
            let name = serde_json::to_value(tier).unwrap();
            let parsed = parse(&[(SmartAutoHeaders::TIER, name.as_str().unwrap())]).unwrap();
            assert_eq!(parsed.policy.tier, Some(tier));
        }
        for (role_name, role) in SmartAutoHeaders::ROLES {
            assert_eq!(serde_json::to_value(role).unwrap(), role_name);
            let parsed = parse(&[(SmartAutoHeaders::ROLE, role_name)]).unwrap();
            assert_eq!(parsed.policy.role, Some(role));
        }
    }

    #[test]
    fn repeated_list_headers_join_one_allow_list() {
        let parsed = parse(&[
            (SmartAutoHeaders::INCLUDE, "copilot/*"),
            (SmartAutoHeaders::INCLUDE, "openai/*"),
            (SmartAutoHeaders::EXCLUDE, "a/*"),
            (SmartAutoHeaders::EXCLUDE, "b/*"),
        ])
        .unwrap();
        assert_eq!(
            parsed.policy.include,
            vec![globs(&["copilot/*", "openai/*"])]
        );
        assert_eq!(parsed.policy.exclude, globs(&["a/*", "b/*"]));
    }

    #[test]
    fn header_layer_only_tightens_an_earlier_layer() {
        let selector = SmartAutoPolicy {
            tier: Some(SmartAutoTier::Balance),
            min_context_tokens: Some(1_000_000),
            ..SmartAutoPolicy::default()
        };
        let parsed = parse(&[
            (SmartAutoHeaders::TIER, "fast"),
            (SmartAutoHeaders::MIN_CONTEXT, "200k"),
            (SmartAutoHeaders::INCLUDE, "copilot/*"),
        ])
        .unwrap();
        let merged = selector.merge(&parsed.policy);
        assert_eq!(merged.tier, Some(SmartAutoTier::Balance));
        assert_eq!(merged.min_context_tokens, Some(1_000_000));
        assert_eq!(merged.include, vec![globs(&["copilot/*"])]);
    }

    #[test]
    fn malformed_values_are_bad_requests() {
        for pairs in [
            [(SmartAutoHeaders::MIN_CONTEXT, "1.5m")],
            [(SmartAutoHeaders::MIN_INPUT, "lots")],
            [(SmartAutoHeaders::MIN_OUTPUT, "-1")],
            [(SmartAutoHeaders::TIER, "max")],
            [(SmartAutoHeaders::TIER, "smart-auto-balance")],
            [(SmartAutoHeaders::ROLE, "coding")],
            [(SmartAutoHeaders::INCLUDE, "copilot/*,,openai/*")],
            [(SmartAutoHeaders::EXCLUDE, " ")],
            [(SmartAutoHeaders::WORKER, "")],
            [("x-llm-hub-smart-auto-min-ctx", "1m")],
        ] {
            assert_bad_request(&pairs);
        }
    }

    #[test]
    fn repeated_single_value_headers_are_bad_requests() {
        assert_bad_request(&[
            (SmartAutoHeaders::MIN_CONTEXT, "1m"),
            (SmartAutoHeaders::MIN_CONTEXT, "200k"),
        ]);
        assert_bad_request(&[
            (SmartAutoHeaders::WORKER, "a"),
            (SmartAutoHeaders::WORKER, "b"),
        ]);
    }

    #[test]
    fn identity_longer_than_the_limit_is_a_bad_request() {
        let long = "w".repeat(SmartAutoHeaders::MAX_IDENTITY_LENGTH + 1);
        assert_bad_request(&[(SmartAutoHeaders::PARENT, &long)]);
        let limit = "w".repeat(SmartAutoHeaders::MAX_IDENTITY_LENGTH);
        let parsed = parse(&[(SmartAutoHeaders::PARENT, &limit)]).unwrap();
        assert_eq!(parsed.parent, Some(limit));
    }

    #[test]
    fn non_ascii_value_is_a_bad_request() {
        let mut map = HeaderMap::new();
        map.insert(
            SmartAutoHeaders::ROLE,
            HeaderValue::from_bytes("planning\u{e9}".as_bytes()).unwrap(),
        );
        assert!(matches!(
            SmartAutoHeaders::from_headers(&map),
            Err(AppError::BadRequest(_))
        ));
    }
}

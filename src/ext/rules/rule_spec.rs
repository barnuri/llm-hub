//! The rules file format. Unknown keys fail the load, so a typo never turns a rule off.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::ext::hooks::HookSpec;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleFile {
    #[serde(default)]
    pub rules: Vec<RuleSpec>,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleSpec {
    pub name: String,
    pub phase: PhaseName,
    #[serde(default)]
    pub mode: RuleMode,
    #[serde(default, rename = "match")]
    pub matches: MatchSpec,
    pub actions: Vec<ActionSpec>,
}

/// Whether a matching rule acts, or only logs that it would have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuleMode {
    #[default]
    Enforce,
    Shadow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PhaseName {
    HttpRequest,
    Request,
    Attempt,
    Response,
    HttpResponse,
    Complete,
}

impl PhaseName {
    pub fn as_str(self) -> &'static str {
        match self {
            PhaseName::HttpRequest => "http_request",
            PhaseName::Request => "request",
            PhaseName::Attempt => "attempt",
            PhaseName::Response => "response",
            PhaseName::HttpResponse => "http_response",
            PhaseName::Complete => "complete",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MatchSpec {
    pub path: Option<OneOrMany<String>>,
    pub method: Option<OneOrMany<String>>,
    pub api: Option<OneOrMany<String>>,
    pub model: Option<OneOrMany<String>>,
    pub profile: Option<OneOrMany<String>>,
    pub header: Option<BTreeMap<String, String>>,
    pub header_absent: Option<OneOrMany<String>>,
    pub stream: Option<bool>,
    pub status: Option<OneOrMany<StatusSpec>>,
    pub error_code: Option<OneOrMany<String>>,
}

/// One value or a list of values.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::One(one) => vec![one],
            OneOrMany::Many(many) => many,
        }
    }
}

/// An exact status such as `400`, or a class such as `"5xx"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum StatusSpec {
    Code(u16),
    Class(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ActionSpec {
    SetHeader(BTreeMap<String, String>),
    RemoveHeader(OneOrMany<String>),
    SetField(BTreeMap<String, Value>),
    RemoveField(OneOrMany<String>),
    SetModel(String),
    SetFallbacks(Vec<String>),
    Reject(RejectSpec),
    RetryNext(bool),
    SetStatus(u16),
}

impl ActionSpec {
    pub fn name(&self) -> &'static str {
        match self {
            ActionSpec::SetHeader(_) => "set_header",
            ActionSpec::RemoveHeader(_) => "remove_header",
            ActionSpec::SetField(_) => "set_field",
            ActionSpec::RemoveField(_) => "remove_field",
            ActionSpec::SetModel(_) => "set_model",
            ActionSpec::SetFallbacks(_) => "set_fallbacks",
            ActionSpec::Reject(_) => "reject",
            ActionSpec::RetryNext(_) => "retry_next",
            ActionSpec::SetStatus(_) => "set_status",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RejectSpec {
    pub status: u16,
    pub message: String,
}

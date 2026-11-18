//! The `hooks:` entries of a rules file. Unknown keys fail the load.

use serde::Deserialize;

use crate::ext::rules::{MatchSpec, PhaseName, RuleMode};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HookSpec {
    pub name: String,
    pub phase: PhaseName,
    pub url: String,
    #[serde(default, rename = "match")]
    pub matches: MatchSpec,
    /// What the hub sends besides the metadata. Default: the headers.
    #[serde(default)]
    pub send: Vec<SendPart>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub fail: FailMode,
    #[serde(default)]
    pub mode: RuleMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SendPart {
    Headers,
    Body,
}

/// What a failed hook call does to the request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailMode {
    /// Skip the hook and go on.
    #[default]
    Open,
    /// Reject the request.
    Closed,
}

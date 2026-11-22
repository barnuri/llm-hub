use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Where an extension comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionKind {
    /// Ships with llm-hub.
    Builtin,
    /// The rules of the rules file, as one extension.
    Rules,
    /// Added by a library build with `Hub::extension`.
    Library,
    /// The HTTP hooks of the rules file, as one extension named `hooks`.
    Hook,
}

/// What turned an extension or a rule off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DisabledBy {
    /// `LLM_HUB_EXTENSIONS_DISABLED`. The API cannot change it.
    Env,
    /// A choice saved from the UI or the API.
    Ui,
}

/// One rule of the rules file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RuleStatus {
    pub name: String,
    /// `http_request`, `request`, `attempt`, `response` or `http_response`.
    pub phase: String,
    /// `enforce` or `shadow`.
    pub mode: String,
    pub enabled: bool,
    pub disabled_by: Option<DisabledBy>,
}

/// One extension of the running hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ExtensionStatus {
    pub name: String,
    pub kind: ExtensionKind,
    /// The phases it runs in. Empty for a library extension, whose phases the hub cannot see.
    pub phases: Vec<String>,
    pub enabled: bool,
    pub disabled_by: Option<DisabledBy>,
    /// The rules of the rules extension. Absent for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<RuleStatus>>,
}

/// Every extension, in the order request phases run them, then the hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ExtensionList {
    pub extensions: Vec<ExtensionStatus>,
}

/// Turns an extension or a rule on or off.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ExtensionToggleInput {
    pub enabled: bool,
}

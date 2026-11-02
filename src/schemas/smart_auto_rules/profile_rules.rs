use serde::{Deserialize, Serialize};

/// The include and exclude globs that bind one profile's models only.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileRules {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

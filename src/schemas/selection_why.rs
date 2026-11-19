use serde::{Deserialize, Serialize};

use crate::schemas::directive_outcome::DirectiveOutcome;
use crate::schemas::ranked_model::RankedModel;
use crate::schemas::role_reason::RoleReason;
use crate::schemas::score_parts::ScoreParts;

/// Why the router chose a model, as codes and numbers only, never prompt text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SelectionWhy {
    /// The rule that set the turn's role.
    #[serde(default)]
    pub role_reason: Option<RoleReason>,
    /// What a model directive in the user text did, `None` without one.
    #[serde(default)]
    pub directive: Option<DirectiveOutcome>,
    /// The score parts of the chosen model.
    #[serde(default)]
    pub score: Option<ScoreParts>,
    /// The best other models for the turn, best first.
    #[serde(default)]
    pub runners_up: Vec<RankedModel>,
}

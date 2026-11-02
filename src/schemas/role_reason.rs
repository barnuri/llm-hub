/// Why the classifier chose a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoleReason {
    /// The caller named the role.
    CallerHint,
    /// The earliest cue in the user text is a planning word.
    PlanningCue,
    /// The earliest cue in the user text is an investigation word.
    InvestigationCue,
    /// The earliest cue in the user text is an implementation word.
    ImplementationCue,
    /// The earliest cue in the user text is a validation word.
    ValidationCue,
    /// The text holds no cue but references a plan, a tasks file or a skill.
    PlanMarker,
    /// The turn carries no user text.
    NoUserText,
    /// The user text holds no cue.
    NoCue,
}

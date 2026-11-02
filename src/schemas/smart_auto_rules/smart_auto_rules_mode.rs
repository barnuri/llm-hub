/// Whether a request's include and exclude tighten the saved rules or replace them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SmartAutoRulesMode {
    /// The request narrows the rules; this is the default.
    #[default]
    Merge,
    /// The request's include and exclude replace the rules, so the request may widen them.
    Override,
}

impl SmartAutoRulesMode {
    /// Reads `merge` or `override`, ignoring ASCII case. Any other text returns `None`.
    #[must_use]
    pub fn from_name(text: &str) -> Option<Self> {
        if text.eq_ignore_ascii_case("merge") {
            Some(Self::Merge)
        } else if text.eq_ignore_ascii_case("override") {
            Some(Self::Override)
        } else {
            None
        }
    }
}

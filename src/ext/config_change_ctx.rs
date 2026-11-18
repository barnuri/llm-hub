/// A setting that an admin call saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigKind {
    /// `POST /api/profiles`: a profile is added or changed. The subject is its name.
    ProfileSaved,
    /// `DELETE /api/profiles/{name}`. The subject is the name.
    ProfileDeleted,
    /// `POST /api/fallbacks`: the default fallback chain.
    DefaultFallbacks,
    /// `POST /api/copilot/prices`: the Copilot price refresh interval.
    CopilotPrices,
    /// `POST /api/keep-warm`: the keep-warm switch and idle cap.
    KeepWarm,
    /// `POST /api/keys`: an API key is created. The subject is its name.
    ApiKeyCreated,
    /// `DELETE /api/keys/{name}`. The subject is the name.
    ApiKeyDeleted,
    /// `PUT /api/smart-auto/rules`: the Smart Auto include and exclude rules.
    SmartAutoRules,
}

/// The `config_change` phase view of one setting a user saves.
#[derive(Debug)]
pub struct ConfigChangeCtx<'a> {
    kind: ConfigKind,
    subject: Option<&'a str>,
}

impl<'a> ConfigChangeCtx<'a> {
    pub(crate) fn new(kind: ConfigKind, subject: Option<&'a str>) -> ConfigChangeCtx<'a> {
        ConfigChangeCtx { kind, subject }
    }

    /// Which setting changes.
    #[must_use]
    pub fn kind(&self) -> ConfigKind {
        self.kind
    }

    /// The profile or key the change names, when it names one.
    #[must_use]
    pub fn subject(&self) -> Option<&str> {
        self.subject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_reports_the_kind_and_the_subject() {
        let named = ConfigChangeCtx::new(ConfigKind::ProfileSaved, Some("groq"));
        let unnamed = ConfigChangeCtx::new(ConfigKind::KeepWarm, None);

        assert_eq!(named.kind(), ConfigKind::ProfileSaved);
        assert_eq!(named.subject(), Some("groq"));
        assert_eq!(unnamed.kind(), ConfigKind::KeepWarm);
        assert_eq!(unnamed.subject(), None);
    }
}

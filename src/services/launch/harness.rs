/// A coding agent that `llm-hub <command>` starts against the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
    Opencode,
    CopilotCli,
    Pi,
}

impl Harness {
    const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Opencode,
        Self::CopilotCli,
        Self::Pi,
    ];

    /// The harness that `llm-hub <command>` names, if any.
    #[must_use]
    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|harness| harness.command() == command)
    }

    /// The `llm-hub` subcommand name.
    #[must_use]
    pub fn command(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::CopilotCli => "copilot-cli",
            Self::Pi => "pi",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_name_round_trips() {
        for harness in Harness::ALL {
            assert_eq!(Harness::from_command(harness.command()), Some(harness));
        }
    }

    #[test]
    fn copilot_stays_free_for_login_and_logout() {
        assert_eq!(Harness::from_command("copilot"), None);
        assert_eq!(
            Harness::from_command("copilot-cli"),
            Some(Harness::CopilotCli)
        );
    }

    #[test]
    fn unknown_command_is_not_a_harness() {
        assert_eq!(Harness::from_command("service"), None);
        assert_eq!(Harness::from_command(""), None);
    }
}

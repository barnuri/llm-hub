/// `llm-hub <harness> [model] [args...]`: the model is the first argument
/// unless it is empty or starts with `-`. All other arguments pass through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchArgs {
    model: Option<String>,
    passthrough: Vec<String>,
}

impl LaunchArgs {
    /// Splits the arguments after the harness name into the model and the rest.
    #[must_use]
    pub fn parse(args: Vec<String>) -> Self {
        let mut args = args.into_iter().peekable();
        let model = args.next_if(|first| !first.is_empty() && !first.starts_with('-'));
        Self {
            model,
            passthrough: args.collect(),
        }
    }

    /// Parses borrowed test arguments.
    #[cfg(test)]
    #[must_use]
    pub fn from_strs(args: &[&str]) -> Self {
        Self::parse(args.iter().map(ToString::to_string).collect())
    }

    /// The model the user named, if any.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The arguments the harness receives unchanged.
    #[must_use]
    pub fn into_passthrough(self) -> Vec<String> {
        self.passthrough
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_plain_argument_is_the_model() {
        let args = LaunchArgs::from_strs(&["copilot/claude-opus-5-5", "-p", "hi"]);

        assert_eq!(args.model(), Some("copilot/claude-opus-5-5"));
        assert_eq!(args.into_passthrough(), ["-p", "hi"]);
    }

    #[test]
    fn leading_flag_means_no_model() {
        let args = LaunchArgs::from_strs(&["-p", "hi"]);

        assert_eq!(args.model(), None);
        assert_eq!(args.into_passthrough(), ["-p", "hi"]);
    }

    #[test]
    fn empty_first_argument_is_not_a_model() {
        let args = LaunchArgs::from_strs(&["", "--resume"]);

        assert_eq!(args.model(), None);
        assert_eq!(args.into_passthrough(), ["", "--resume"]);
    }

    #[test]
    fn no_arguments_means_no_model_and_nothing_to_pass() {
        let args = LaunchArgs::from_strs(&[]);

        assert_eq!(args.model(), None);
        assert!(args.into_passthrough().is_empty());
    }
}

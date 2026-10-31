/// `llm-hub <harness> [model] [args...]`: the model is the first argument
/// unless it is empty or starts with `-`. All other arguments pass through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchArgs {
    model: Option<String>,
    passthrough: Vec<String>,
}

impl LaunchArgs {
    #[must_use]
    pub fn parse(args: Vec<String>) -> Self {
        let mut args = args.into_iter().peekable();
        let model = args.next_if(|first| !first.is_empty() && !first.starts_with('-'));
        Self {
            model,
            passthrough: args.collect(),
        }
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn into_passthrough(self) -> Vec<String> {
        self.passthrough
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> LaunchArgs {
        LaunchArgs::parse(args.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn first_plain_argument_is_the_model() {
        let args = parse(&["copilot/claude-opus-5-5", "-p", "hi"]);

        assert_eq!(args.model(), Some("copilot/claude-opus-5-5"));
        assert_eq!(args.into_passthrough(), ["-p", "hi"]);
    }

    #[test]
    fn leading_flag_means_no_model() {
        let args = parse(&["-p", "hi"]);

        assert_eq!(args.model(), None);
        assert_eq!(args.into_passthrough(), ["-p", "hi"]);
    }

    #[test]
    fn empty_first_argument_is_not_a_model() {
        let args = parse(&["", "--resume"]);

        assert_eq!(args.model(), None);
        assert_eq!(args.into_passthrough(), ["", "--resume"]);
    }

    #[test]
    fn no_arguments_means_no_model_and_nothing_to_pass() {
        let args = parse(&[]);

        assert_eq!(args.model(), None);
        assert!(args.into_passthrough().is_empty());
    }
}

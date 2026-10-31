use std::process::Command;

/// A harness command line: the program, its argv, and the env it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl LaunchSpec {
    #[must_use]
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(args);
        self
    }

    #[must_use]
    pub fn env(mut self, name: &str, value: impl Into<String>) -> Self {
        self.env.push((name.to_string(), value.into()));
        self
    }

    #[cfg(test)]
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[cfg(test)]
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.args
    }

    /// The value the spec sets for `name`, if it sets one.
    #[cfg(test)]
    #[must_use]
    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[cfg(test)]
    #[must_use]
    pub fn env_names(&self) -> Vec<&str> {
        self.env.iter().map(|(key, _)| key.as_str()).collect()
    }

    /// Replaces this process with the harness, so it owns the terminal and
    /// its signals. On Windows the hub waits and exits with the child's code.
    ///
    /// # Errors
    /// Returns a message when the harness binary cannot start.
    pub fn exec(self) -> Result<(), String> {
        let mut command = Command::new(&self.program);
        command.args(&self.args).envs(self.env);
        Self::replace_process(&mut command)
            .map_err(|e| format!("cannot start {}: {e}", self.program))
    }

    #[cfg(unix)]
    fn replace_process(command: &mut Command) -> std::io::Result<()> {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
    }

    #[cfg(not(unix))]
    fn replace_process(command: &mut Command) -> std::io::Result<()> {
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

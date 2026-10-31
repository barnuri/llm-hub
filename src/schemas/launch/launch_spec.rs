use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

/// A harness command line: the program, its argv, and the env it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl LaunchSpec {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

    /// A spec that runs `program` with no arguments and no added env.
    #[must_use]
    pub fn new(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends every argument in order.
    #[must_use]
    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args.extend(args);
        self
    }

    /// Sets `name` in the harness env.
    #[must_use]
    pub fn env(mut self, name: &str, value: impl Into<String>) -> Self {
        self.env.push((name.to_string(), value.into()));
        self
    }

    /// The program name the spec starts.
    #[cfg(test)]
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments after the program.
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

    /// The env names the spec sets, in order.
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
        let mut command = Command::new(self.program_path());
        command.args(&self.args).envs(self.env);
        Self::replace_process(&mut command)
            .map_err(|e| format!("cannot start {}: {e}", self.program))
    }

    /// Windows only finds a bare name as `.exe`, so npm `.cmd` shims are looked up here.
    /// std runs a `.cmd`/`.bat` path through `cmd.exe` with batch-safe argument quoting.
    fn program_path(&self) -> PathBuf {
        if !cfg!(windows) {
            return PathBuf::from(&self.program);
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| Self::DEFAULT_PATHEXT.to_string());
        Self::find_on_path(&self.program, &path, &extensions)
            .unwrap_or_else(|| PathBuf::from(&self.program))
    }

    /// The first `<dir>/<program><ext>` file, by `PATH` order then `PATHEXT` order.
    fn find_on_path(program: &str, path: &OsStr, extensions: &str) -> Option<PathBuf> {
        let extensions: Vec<&str> = extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .collect();
        std::env::split_paths(path)
            .flat_map(|dir| {
                extensions
                    .iter()
                    .map(move |extension| dir.join(format!("{program}{extension}")))
            })
            .find(|candidate| candidate.is_file())
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::*;

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let file = dir.join(name);
        std::fs::write(&file, "").unwrap();
        file
    }

    #[test]
    fn first_extension_that_exists_wins() {
        let dir = tempfile::tempdir().unwrap();
        let shim = touch(dir.path(), "tool.cmd");
        touch(dir.path(), "tool.bat");

        let found = LaunchSpec::find_on_path("tool", &path_of(&[dir.path()]), ".exe;.cmd;.bat");

        assert_eq!(found, Some(shim));
    }

    #[test]
    fn earlier_path_dir_wins_over_a_preferred_extension() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let shim = touch(first.path(), "tool.cmd");
        touch(second.path(), "tool.exe");

        let found = LaunchSpec::find_on_path(
            "tool",
            &path_of(&[first.path(), second.path()]),
            ".exe;.cmd",
        );

        assert_eq!(found, Some(shim));
    }

    #[test]
    fn missing_program_and_dirs_are_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tool.exe")).unwrap();

        let path = path_of(&[dir.path(), &dir.path().join("absent")]);

        assert_eq!(LaunchSpec::find_on_path("tool", &path, ".exe;;.cmd"), None);
    }

    #[cfg(windows)]
    #[test]
    fn cmd_shim_runs_through_cmd_exe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool.cmd"), "@exit /b 3\r\n").unwrap();
        let shim = LaunchSpec::find_on_path("tool", &path_of(&[dir.path()]), ".exe;.cmd").unwrap();

        let status = Command::new(shim).arg("a&b").status().unwrap();

        assert_eq!(status.code(), Some(3));
    }
}

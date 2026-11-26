use std::fs;
use std::path::{Path, PathBuf};

/// What `llm-hub setup <harness>` does: write one config file, or tell the user what to set.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupOutcome {
    Write(SetupPlan),
    Manual(String),
}

impl SetupOutcome {
    /// Carries the outcome out, or only describes it when `dry_run` is set.
    ///
    /// # Errors
    /// Returns a message when the config file cannot be written.
    pub fn apply(&self, dry_run: bool) -> Result<String, String> {
        match self {
            Self::Write(plan) => plan.apply(dry_run),
            Self::Manual(text) => Ok(text.clone()),
        }
    }
}

/// The full new content of one harness config file.
#[derive(Debug, PartialEq, Eq)]
pub struct SetupPlan {
    pub path: PathBuf,
    pub content: String,
}

impl SetupPlan {
    /// The file's text, or `None` when it does not exist yet.
    ///
    /// # Errors
    /// Returns a message when the file exists but cannot be read.
    pub fn read_existing(path: &Path) -> Result<Option<String>, String> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Writes the content through a temp file and a rename. A file that already holds the
    /// content stays untouched. `dry_run` writes nothing and returns the content.
    ///
    /// # Errors
    /// Returns a message when the directory, temp file or rename fails.
    pub fn apply(&self, dry_run: bool) -> Result<String, String> {
        let shown = self.path.display();
        if dry_run {
            return Ok(format!("dry run, would write {shown}:\n{}", self.content));
        }
        if Self::read_existing(&self.path)?.as_deref() == Some(self.content.as_str()) {
            return Ok(format!("{shown} is already up to date"));
        }
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let name = self
            .path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let temp = dir.join(format!(".{name}.llm-hub-tmp"));
        fs::write(&temp, &self.content)
            .map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
        fs::rename(&temp, &self.path).map_err(|e| format!("cannot replace {shown}: {e}"))?;
        Ok(format!("wrote {shown}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(dir: &Path, content: &str) -> SetupPlan {
        SetupPlan {
            path: dir.join("a/b.json"),
            content: content.to_string(),
        }
    }

    #[test]
    fn apply_creates_the_file_and_its_directories() {
        let dir = tempfile::tempdir().unwrap();

        let message = plan(dir.path(), "x").apply(false).unwrap();

        assert!(message.starts_with("wrote "));
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b.json")).unwrap(),
            "x"
        );
        assert!(!dir.path().join("a/.b.json.llm-hub-tmp").exists());
    }

    #[test]
    fn apply_twice_leaves_the_second_run_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        plan(dir.path(), "x").apply(false).unwrap();

        let message = plan(dir.path(), "x").apply(false).unwrap();

        assert!(message.ends_with("is already up to date"));
    }

    #[test]
    fn apply_replaces_other_content() {
        let dir = tempfile::tempdir().unwrap();
        plan(dir.path(), "x").apply(false).unwrap();

        plan(dir.path(), "y").apply(false).unwrap();

        assert_eq!(
            fs::read_to_string(dir.path().join("a/b.json")).unwrap(),
            "y"
        );
    }

    #[test]
    fn dry_run_shows_the_content_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();

        let message = SetupOutcome::Write(plan(dir.path(), "body"))
            .apply(true)
            .unwrap();

        assert!(message.starts_with("dry run, would write "));
        assert!(message.ends_with("\nbody"));
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn manual_outcome_is_the_text() {
        assert_eq!(
            SetupOutcome::Manual("t".into()).apply(false),
            Ok("t".to_string())
        );
    }

    #[test]
    fn read_existing_tells_missing_from_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        assert_eq!(SetupPlan::read_existing(&path), Ok(None));
        fs::write(&path, "t").unwrap();
        assert_eq!(SetupPlan::read_existing(&path), Ok(Some("t".to_string())));
        assert!(SetupPlan::read_existing(dir.path()).is_err());
    }
}

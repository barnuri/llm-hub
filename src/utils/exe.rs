use std::path::PathBuf;

/// The running executable with symlinks resolved, so a link on `PATH` into a
/// source checkout is still recognised as a source install.
///
/// # Errors
/// Returns the error from `std::env::current_exe` when the OS cannot report it.
pub fn current_exe_resolved() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(resolve_links)
}

fn resolve_links(exe: PathBuf) -> PathBuf {
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::source_install::find_source_repo;

    #[cfg(unix)]
    #[test]
    fn link_on_path_into_a_checkout_resolves_to_the_source_repo() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname = \"llm-hub\"\n",
        )
        .unwrap();
        let binary = repo.path().join("target/release/llm-hub");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let link = bin_dir.path().join("llm-hub");
        std::os::unix::fs::symlink(&binary, &link).unwrap();

        let resolved = resolve_links(link.clone());

        assert!(find_source_repo(&link).is_none());
        assert_eq!(
            find_source_repo(&resolved),
            Some(std::fs::canonicalize(repo.path()).unwrap())
        );
    }

    #[test]
    fn missing_path_is_returned_unchanged() {
        let missing = PathBuf::from("/nonexistent/llm-hub");

        assert_eq!(resolve_links(missing.clone()), missing);
    }
}

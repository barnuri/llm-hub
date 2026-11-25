use std::path::{Path, PathBuf};

/// The running executable with symlinks resolved, so a link on `PATH` into a
/// source checkout is still recognised as a source install.
///
/// # Errors
/// Returns the error from `std::env::current_exe` when the OS cannot report it.
pub fn current_exe_resolved() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(resolve_links)
}

/// The path a service definition should run. A binary in `<releases>/<sha>/`
/// whose sibling `<releases>/current` link points at that directory runs as
/// `<releases>/current/<binary>`, so a deploy swaps the link and a later dev
/// build never changes the service. Any other path is returned unchanged.
#[must_use]
pub fn stable_service_exe(resolved: &Path) -> PathBuf {
    let (Some(dir), Some(name)) = (resolved.parent(), resolved.file_name()) else {
        return resolved.to_path_buf();
    };
    let Some(releases) = dir.parent() else {
        return resolved.to_path_buf();
    };
    let current = releases.join("current");
    let points_here = std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.is_symlink())
        && std::fs::canonicalize(&current)
            .is_ok_and(|target| Some(target) == std::fs::canonicalize(dir).ok());
    if points_here {
        current.join(name)
    } else {
        resolved.to_path_buf()
    }
}

fn resolve_links(exe: PathBuf) -> PathBuf {
    std::fs::canonicalize(&exe).unwrap_or(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn link_on_path_into_a_checkout_resolves_to_the_source_repo() {
        use crate::services::source_install::find_source_repo;

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

    #[cfg(unix)]
    #[test]
    fn release_binary_behind_current_link_runs_through_the_link() {
        let releases = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(releases.path()).unwrap();
        let release = root.join("abc123");
        std::fs::create_dir(&release).unwrap();
        std::fs::write(release.join("llm-hub"), "").unwrap();
        std::os::unix::fs::symlink(&release, root.join("current")).unwrap();

        assert_eq!(
            stable_service_exe(&release.join("llm-hub")),
            root.join("current/llm-hub")
        );
    }

    #[cfg(unix)]
    #[test]
    fn release_not_selected_by_current_keeps_its_own_path() {
        let releases = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(releases.path()).unwrap();
        for sha in ["old", "new"] {
            std::fs::create_dir(root.join(sha)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("new"), root.join("current")).unwrap();
        let old = root.join("old/llm-hub");

        assert_eq!(stable_service_exe(&old), old);
    }

    #[test]
    fn dev_build_path_is_unchanged_and_not_a_release() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("target/release/llm-hub");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "").unwrap();

        assert_eq!(stable_service_exe(&binary), binary);
        assert_eq!(
            stable_service_exe(Path::new("llm-hub")),
            PathBuf::from("llm-hub")
        );
    }

    #[test]
    fn missing_path_is_returned_unchanged() {
        let missing = PathBuf::from("/nonexistent/llm-hub");

        assert_eq!(resolve_links(missing.clone()), missing);
    }
}

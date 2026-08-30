use std::path::{Path, PathBuf};

const PACKAGE_NAME: &str = "llm-hub";

/// Walks ancestors of the running executable's path looking for the llm-hub
/// source checkout: a directory holding both `.git` and a `Cargo.toml` whose
/// `[package]` name is `llm-hub`. Returns the repo root when found.
pub fn find_source_repo(exe_path: &Path) -> Option<PathBuf> {
    exe_path
        .ancestors()
        .find(|dir| is_repo_root(dir))
        .map(Path::to_path_buf)
}

fn is_repo_root(dir: &Path) -> bool {
    if !dir.join(".git").exists() {
        return false;
    }
    let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    manifest_names_package(&manifest)
}

fn manifest_names_package(manifest: &str) -> bool {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        return value.trim().trim_matches('"') == PACKAGE_NAME;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(root: &Path, package_name: &str) {
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
    }

    #[test]
    fn finds_repo_from_target_release_exe() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("llm-hub");
        make_repo(&repo, "llm-hub");
        let exe = repo.join("target/release/llm-hub");

        assert_eq!(find_source_repo(&exe), Some(repo));
    }

    #[test]
    fn none_when_git_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("llm-hub");
        make_repo(&repo, "llm-hub");
        std::fs::remove_dir(repo.join(".git")).unwrap();
        let exe = repo.join("target/release/llm-hub");

        assert_eq!(find_source_repo(&exe), None);
    }

    #[test]
    fn none_when_cargo_toml_names_other_package() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("llm-hub");
        make_repo(&repo, "other-crate");
        let exe = repo.join("target/release/llm-hub");

        assert_eq!(find_source_repo(&exe), None);
    }

    #[test]
    fn nearest_ancestor_wins() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        make_repo(&outer, "llm-hub");
        let inner = outer.join("vendor/llm-hub");
        make_repo(&inner, "llm-hub");
        let exe = inner.join("target/release/llm-hub");

        assert_eq!(find_source_repo(&exe), Some(inner));
    }
}

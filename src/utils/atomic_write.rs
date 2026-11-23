use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to a temp file beside `path`, syncs it and renames it over `path`,
/// so readers see the old or the new content, never half. It removes the temp file on error.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = temp_path(path);
    write_and_sync(&temp, bytes)
        .and_then(|()| std::fs::rename(&temp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })
}

fn write_and_sync(temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(temp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Names the temp file with the pid and a per-process counter, so concurrent writers never share one.
fn temp_path(path: &Path) -> PathBuf {
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy());
    let name = name.trim_start_matches('.');
    dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn creates_a_new_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write_atomically(&path, b"hello\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
        assert_eq!(entries(dir.path()), vec!["out.txt".to_string()]);
    }

    #[test]
    fn replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"old content that is longer").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(entries(dir.path()), vec!["out.txt".to_string()]);
    }

    #[test]
    fn removes_the_temp_file_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        // A non-empty directory at the target makes the rename fail on every platform.
        let path = dir.path().join("target");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"x").unwrap();
        assert!(write_atomically(&path, b"data").is_err());
        assert_eq!(entries(dir.path()), vec!["target".to_string()]);
    }

    #[test]
    fn fails_when_the_directory_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("out.txt");
        assert!(write_atomically(&path, b"data").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn temp_names_are_unique_and_beside_the_target() {
        let path = Path::new("/some/dir/.env");
        let first = temp_path(path);
        let second = temp_path(path);
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(Path::new("/some/dir")));
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!(".env.tmp-{}-", std::process::id())));
    }

    #[test]
    fn temp_path_uses_the_current_dir_for_a_bare_name() {
        let temp = temp_path(Path::new("out.txt"));
        assert_eq!(temp.parent(), Some(Path::new(".")));
        let root = temp_path(Path::new("/"));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".file.tmp-")
        );
    }
}

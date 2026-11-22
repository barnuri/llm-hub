use std::path::PathBuf;
use std::process::Command;

/// Sets `LLM_HUB_VERSION`: the version CI stamps into Cargo.toml, or for an
/// unstamped local build (patch 0) the version `git describe` reports.
fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    // A missing path reruns this script on every build, so watch only the files git really has:
    // a linked worktree keeps HEAD in its own git dir and the refs in the common dir.
    if let (Some(git_dir), Some(common_dir)) = (git_path("--git-dir"), git_path("--git-common-dir"))
    {
        for path in [
            git_dir.join("HEAD"),
            common_dir.join("refs/heads"),
            common_dir.join("refs/tags"),
            common_dir.join("packed-refs"),
        ] {
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }

    let stamped = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = if is_unstamped(&stamped) {
        git_version().unwrap_or(stamped)
    } else {
        stamped
    };
    println!("cargo:rustc-env=LLM_HUB_VERSION={version}");
}

fn is_unstamped(version: &str) -> bool {
    version.rsplit('.').next() == Some("0")
}

/// An absolute path that `git rev-parse <flag>` prints, or `None` outside a git checkout.
fn git_path(flag: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", flag])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}

/// `v0.1.30-3-g842543c` becomes `0.1.30+3.g842543c`; an exact tag becomes `0.1.30`.
fn git_version() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--long", "--match", "v[0-9]*"])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let described = String::from_utf8(output.stdout).ok()?;
    let mut parts = described.trim().trim_start_matches('v').rsplitn(3, '-');
    let hash = parts.next()?;
    let ahead = parts.next()?;
    let tag = parts.next()?;
    Some(if ahead == "0" {
        tag.to_string()
    } else {
        format!("{tag}+{ahead}.{hash}")
    })
}

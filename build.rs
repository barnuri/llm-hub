use std::process::Command;

/// Sets `LLM_HUB_VERSION`: the version CI stamps into Cargo.toml, or for an
/// unstamped local build (patch 0) the version `git describe` reports.
fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
    println!("cargo:rerun-if-changed=.git/refs/tags");
    println!("cargo:rerun-if-changed=.git/packed-refs");

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

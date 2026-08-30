use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::consts::{GITHUB_REPO, UPDATE_CHECK_INTERVAL, VERSION};
use crate::services::source_install;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// Distinguishes "a new build/binary was put in place" from "nothing changed",
/// so the auto-update loop only restarts when an update was really applied.
#[derive(PartialEq)]
pub enum UpdateOutcome {
    Applied,
    AlreadyUpToDate,
}

/// `llm-hub update`: source checkouts update via `git pull` + `cargo build`,
/// everything else via the latest GitHub release binary.
pub async fn self_update() -> Result<UpdateOutcome, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    if let Some(repo) = source_install::find_source_repo(&current) {
        return update_from_source(&repo, &current).await;
    }

    let release = fetch_latest().await?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    if latest == VERSION {
        println!("already up to date (v{VERSION})");
        return Ok(UpdateOutcome::AlreadyUpToDate);
    }
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_asset_name())
        .ok_or_else(|| {
            format!(
                "no release asset named {} in v{latest}",
                expected_asset_name()
            )
        })?;

    println!("updating v{VERSION} -> v{latest} ...");
    let bytes = http_client()?
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;

    replace_current_binary(&bytes)?;
    println!("updated to v{latest} — restart llm-hub to use it");
    Ok(UpdateOutcome::Applied)
}

/// Checks at startup and every UPDATE_CHECK_INTERVAL. With auto-update
/// enabled it applies new releases; otherwise it only logs that one exists.
/// Never blocks serving.
pub fn spawn_auto_update(auto_update_enabled: bool, restart_notify: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        loop {
            check_and_maybe_update(auto_update_enabled, &restart_notify).await;
            tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
        }
    });
}

async fn check_and_maybe_update(auto_update_enabled: bool, restart_notify: &tokio::sync::Notify) {
    let current = match std::env::current_exe() {
        Ok(current) => current,
        Err(e) => {
            tracing::debug!("update check failed: {e}");
            return;
        }
    };
    if let Some(repo) = source_install::find_source_repo(&current) {
        check_source_update(auto_update_enabled, restart_notify, &repo, &current).await;
        return;
    }

    let release = match fetch_latest().await {
        Ok(release) => release,
        Err(e) => {
            tracing::debug!("update check failed: {e}");
            return;
        }
    };
    let latest = release.tag_name.trim_start_matches('v');
    if latest == VERSION {
        return;
    }
    if !auto_update_enabled {
        tracing::info!(
            "new version available: v{latest} (running v{VERSION}) — run `llm-hub update`"
        );
        return;
    }
    match self_update().await {
        Err(e) => {
            tracing::warn!("auto-update to v{latest} failed: {e}");
            return;
        }
        Ok(UpdateOutcome::AlreadyUpToDate) => {
            tracing::debug!("no update to apply (running v{VERSION}, latest release v{latest})");
            return;
        }
        Ok(UpdateOutcome::Applied) => {}
    }
    notify_or_log_restart(restart_notify, &format!("auto-updated to v{latest}"));
}

/// Source installs are stale when upstream has new commits, not when a new
/// release tag exists — so they get their own check that skips the tag gate.
async fn check_source_update(
    auto_update_enabled: bool,
    restart_notify: &tokio::sync::Notify,
    repo: &Path,
    current: &Path,
) {
    if !auto_update_enabled {
        tracing::debug!("auto-update disabled — run `llm-hub update` to pull + rebuild");
        return;
    }
    match worktree_dirty(repo).await {
        Err(e) => {
            tracing::debug!("source auto-update skipped: {e}");
            return;
        }
        Ok(true) => {
            tracing::info!(
                "source auto-update skipped: uncommitted changes in {} — commit/stash them or run `llm-hub update`",
                repo.display()
            );
            return;
        }
        Ok(false) => {}
    }
    match update_from_source(repo, current).await {
        Err(e) => tracing::warn!("source auto-update failed: {e}"),
        Ok(UpdateOutcome::AlreadyUpToDate) => {
            tracing::debug!("source checkout already up to date");
        }
        Ok(UpdateOutcome::Applied) => {
            notify_or_log_restart(restart_notify, "auto-updated from source");
        }
    }
}

fn notify_or_log_restart(restart_notify: &tokio::sync::Notify, what: &str) {
    if std::env::var("LLM_HUB_SERVICE").as_deref() == Ok("1") {
        tracing::info!("{what} — restarting so the service loads it");
        restart_notify.notify_one();
        return;
    }
    tracing::info!("{what} — restart llm-hub to use it");
}

/// Update path for git/source installs: fast-forward the checkout, rebuild,
/// and make sure the running binary path ends up with the fresh build.
async fn update_from_source(repo: &Path, current: &Path) -> Result<UpdateOutcome, String> {
    println!(
        "source install detected at {} — updating via git pull + cargo build ...",
        repo.display()
    );
    run_in_repo(repo, "git", &["pull", "--ff-only"], PULL_HINT).await?;
    let head = git_head(repo).await?;
    if installed_head(repo).as_deref() == Some(head.as_str()) {
        println!("source checkout already up to date");
        return Ok(UpdateOutcome::AlreadyUpToDate);
    }
    run_in_repo(repo, "cargo", &["build", "--release"], BUILD_HINT).await?;

    let built = repo.join("target").join("release").join(binary_name());
    if !is_same_file(current, &built) {
        let bytes = std::fs::read(&built)
            .map_err(|e| format!("read fresh build {} failed: {e}", built.display()))?;
        replace_current_binary(&bytes)?;
    }
    record_installed_head(repo, &head);
    println!("updated from source — restart llm-hub to use it");
    Ok(UpdateOutcome::Applied)
}

/// The commit the currently installed binary was built from. Kept in a marker
/// file (not just pre/post-pull HEAD comparison) so a failed build retries on
/// the next attempt instead of being mistaken for "already up to date".
fn installed_head_marker(repo: &Path) -> std::path::PathBuf {
    repo.join("target").join(".llm-hub-installed-head")
}

fn installed_head(repo: &Path) -> Option<String> {
    let head = std::fs::read_to_string(installed_head_marker(repo)).ok()?;
    Some(head.trim().to_string())
}

fn record_installed_head(repo: &Path, head: &str) {
    let marker = installed_head_marker(repo);
    if let Err(e) = std::fs::write(&marker, head) {
        tracing::debug!(
            "failed to record installed commit at {}: {e}",
            marker.display()
        );
    }
}

/// Current commit of the checkout, recorded so a later check can tell whether
/// the installed binary is built from HEAD.
async fn git_head(repo: &Path) -> Result<String, String> {
    let stdout = git_capture(repo, &["rev-parse", "HEAD"]).await?;
    Ok(stdout.trim().to_string())
}

/// The background loop never pulls over uncommitted work; a human running
/// `llm-hub update` still can (git itself surfaces any conflict).
async fn worktree_dirty(repo: &Path) -> Result<bool, String> {
    let stdout = git_capture(repo, &["status", "--porcelain"]).await?;
    Ok(!stdout.trim().is_empty())
}

async fn git_capture(repo: &Path, args: &'static [&'static str]) -> Result<String, String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "git not found on PATH — required to update a source install".to_string()
                } else {
                    format!("failed to run git: {e}")
                }
            })?;
        if !output.status.success() {
            return Err(format!(
                "`git {}` failed ({}) in {}",
                args.join(" "),
                output.status,
                repo.display()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .map_err(|e| e.to_string())?
}

const PULL_HINT: &str = "commit/stash local changes or reset a diverged branch, then retry";
const BUILD_HINT: &str = "fix the build in the checkout, then retry";

/// Runs `program args..` inside the repo with inherited stdio so the user
/// sees git/cargo progress. Distinguishes missing tools from failed runs.
async fn run_in_repo(
    repo: &Path,
    program: &'static str,
    args: &'static [&'static str],
    failure_hint: &'static str,
) -> Result<(), String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let status = std::process::Command::new(program)
            .args(args)
            .current_dir(&repo)
            .status()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("{program} not found on PATH — required to update a source install")
                } else {
                    format!("failed to run {program}: {e}")
                }
            })?;
        if status.success() {
            return Ok(());
        }
        Err(format!(
            "`{program} {}` failed ({status}) in {} — {failure_hint}",
            args.join(" "),
            repo.display()
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn binary_name() -> String {
    format!("llm-hub{}", std::env::consts::EXE_SUFFIX)
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

async fn fetch_latest() -> Result<Release, String> {
    http_client()?
        .get(format!(
            "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<Release>()
        .await
        .map_err(|e| e.to_string())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("llm-hub/{VERSION}"))
        .build()
        .map_err(|e| e.to_string())
}

fn expected_asset_name() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("llm-hub-{os}-{arch}")
}

/// Writes the new binary next to the current one, then atomically renames
/// over it. The running process keeps executing the old inode.
fn replace_current_binary(bytes: &[u8]) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = current.with_extension("update-tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write update failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }
    std::fs::rename(&tmp, &current).map_err(|e| format!("replace binary failed: {e}"))
}

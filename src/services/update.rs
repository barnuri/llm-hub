use std::path::{Path, PathBuf};
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
#[derive(Debug, PartialEq)]
pub enum UpdateOutcome {
    Applied,
    AlreadyUpToDate,
}

/// `llm-hub update`: source checkouts update via `git pull` + `cargo build`,
/// everything else via the latest GitHub release binary.
pub async fn self_update() -> Result<UpdateOutcome, String> {
    let current = crate::utils::exe::current_exe_resolved().map_err(|e| e.to_string())?;
    Updater::system(current).self_update().await
}

/// Checks at startup and every `UPDATE_CHECK_INTERVAL`. With auto-update
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
    let current = match crate::utils::exe::current_exe_resolved() {
        Ok(current) => current,
        Err(e) => {
            tracing::debug!("update check failed: {e}");
            return;
        }
    };
    Updater::system(current)
        .check_and_maybe_update(auto_update_enabled, restart_notify)
        .await;
}

/// The `cargo build` arguments for a source rebuild. A binary built with the
/// `mistralrs` feature keeps it, so the rebuilt binary has the same engines.
fn cargo_build_args() -> &'static [&'static str] {
    let mistralrs = cfg!(feature = "mistralrs");
    cargo_build_args_for(mistralrs)
}

fn cargo_build_args_for(mistralrs: bool) -> &'static [&'static str] {
    if mistralrs {
        &["build", "--release", "--features", "mistralrs"]
    } else {
        &["build", "--release"]
    }
}

/// Runs git and cargo inside the source checkout.
trait RepoCommands {
    async fn run(
        &self,
        repo: &Path,
        program: &'static str,
        args: &'static [&'static str],
        failure_hint: &'static str,
    ) -> Result<(), String>;

    async fn capture_git(
        &self,
        repo: &Path,
        args: &'static [&'static str],
    ) -> Result<String, String>;
}

/// Spawns the real git and cargo.
struct SystemCommands;

impl RepoCommands for SystemCommands {
    async fn run(
        &self,
        repo: &Path,
        program: &'static str,
        args: &'static [&'static str],
        failure_hint: &'static str,
    ) -> Result<(), String> {
        run_in_repo(repo, program, args, failure_hint).await
    }

    async fn capture_git(
        &self,
        repo: &Path,
        args: &'static [&'static str],
    ) -> Result<String, String> {
        git_capture(repo, args).await
    }
}

/// The running binary, where releases come from, and how git/cargo and the
/// supervisor check are reached.
struct Updater<C> {
    current: PathBuf,
    latest_release_url: String,
    commands: C,
    is_supervised: fn() -> bool,
}

impl Updater<SystemCommands> {
    fn system(current: PathBuf) -> Self {
        Self {
            current,
            latest_release_url: format!(
                "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
            ),
            commands: SystemCommands,
            is_supervised: crate::services::restart::is_supervised,
        }
    }
}

impl<C: RepoCommands> Updater<C> {
    async fn self_update(&self) -> Result<UpdateOutcome, String> {
        if let Some(repo) = source_install::find_source_repo(&self.current) {
            return self.update_from_source(&repo).await;
        }

        let release = self.fetch_latest().await?;
        let latest = release.tag_name.trim_start_matches('v').to_string();
        if latest == release_version() {
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

        replace_binary(&self.current, &bytes)?;
        println!("updated to v{latest} — restart llm-hub to use it");
        Ok(UpdateOutcome::Applied)
    }

    async fn check_and_maybe_update(
        &self,
        auto_update_enabled: bool,
        restart_notify: &tokio::sync::Notify,
    ) {
        if let Some(repo) = source_install::find_source_repo(&self.current) {
            self.check_source_update(auto_update_enabled, restart_notify, &repo)
                .await;
            return;
        }

        let release = match self.fetch_latest().await {
            Ok(release) => release,
            Err(e) => {
                tracing::debug!("update check failed: {e}");
                return;
            }
        };
        let latest = release.tag_name.trim_start_matches('v');
        if latest == release_version() {
            return;
        }
        if !auto_update_enabled {
            tracing::info!(
                "new version available: v{latest} (running v{VERSION}) — run `llm-hub update`"
            );
            return;
        }
        match self.self_update().await {
            Err(e) => {
                tracing::warn!("auto-update to v{latest} failed: {e}");
                return;
            }
            Ok(UpdateOutcome::AlreadyUpToDate) => {
                tracing::debug!(
                    "no update to apply (running v{VERSION}, latest release v{latest})"
                );
                return;
            }
            Ok(UpdateOutcome::Applied) => {}
        }
        self.notify_or_log_restart(restart_notify, &format!("auto-updated to v{latest}"));
    }

    /// Source installs are stale when upstream has new commits, not when a new
    /// release tag exists — so they get their own check that skips the tag gate.
    async fn check_source_update(
        &self,
        auto_update_enabled: bool,
        restart_notify: &tokio::sync::Notify,
        repo: &Path,
    ) {
        if !auto_update_enabled {
            tracing::debug!("auto-update disabled — run `llm-hub update` to pull + rebuild");
            return;
        }
        match self.worktree_dirty(repo).await {
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
        match self.update_from_source(repo).await {
            Err(e) => tracing::warn!("source auto-update failed: {e}"),
            Ok(UpdateOutcome::AlreadyUpToDate) => {
                tracing::debug!("source checkout already up to date");
            }
            Ok(UpdateOutcome::Applied) => {
                self.notify_or_log_restart(restart_notify, "auto-updated from source");
            }
        }
    }

    fn notify_or_log_restart(&self, restart_notify: &tokio::sync::Notify, what: &str) {
        if (self.is_supervised)() {
            tracing::info!("{what} — restarting so the service loads it");
            restart_notify.notify_one();
            return;
        }
        tracing::info!("{what} — restart llm-hub to use it");
    }

    /// Update path for git/source installs: fast-forward the checkout, rebuild,
    /// and make sure the running binary path ends up with the fresh build.
    async fn update_from_source(&self, repo: &Path) -> Result<UpdateOutcome, String> {
        println!(
            "source install detected at {} — updating via git pull + cargo build ...",
            repo.display()
        );
        self.commands
            .run(repo, "git", &["pull", "--ff-only"], PULL_HINT)
            .await?;
        let head = self.git_head(repo).await?;
        if installed_head(repo).as_deref() == Some(head.as_str()) {
            println!("source checkout already up to date");
            return Ok(UpdateOutcome::AlreadyUpToDate);
        }
        let built = repo.join("target").join("release").join(binary_name());
        let set_aside = set_aside_if_running_build(&self.current, &built)?;
        if let Err(e) = self
            .commands
            .run(repo, "cargo", cargo_build_args(), BUILD_HINT)
            .await
        {
            restore_set_aside(set_aside.as_deref(), &built);
            return Err(e);
        }

        if !is_same_file(&self.current, &built) {
            let bytes = std::fs::read(&built)
                .map_err(|e| format!("read fresh build {} failed: {e}", built.display()))?;
            replace_binary(&self.current, &bytes)?;
        }
        record_installed_head(repo, &head);
        println!("updated from source — restart llm-hub to use it");
        Ok(UpdateOutcome::Applied)
    }

    /// Current commit of the checkout, recorded so a later check can tell whether
    /// the installed binary is built from HEAD.
    async fn git_head(&self, repo: &Path) -> Result<String, String> {
        let stdout = self
            .commands
            .capture_git(repo, &["rev-parse", "HEAD"])
            .await?;
        Ok(stdout.trim().to_string())
    }

    /// The background loop never pulls over uncommitted work; a human running
    /// `llm-hub update` still can (git itself surfaces any conflict).
    async fn worktree_dirty(&self, repo: &Path) -> Result<bool, String> {
        let stdout = self
            .commands
            .capture_git(repo, &["status", "--porcelain"])
            .await?;
        Ok(!stdout.trim().is_empty())
    }

    async fn fetch_latest(&self) -> Result<Release, String> {
        http_client()?
            .get(&self.latest_release_url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json::<Release>()
            .await
            .map_err(|e| e.to_string())
    }
}

/// The commit the currently installed binary was built from. Kept in a marker
/// file (not just pre/post-pull HEAD comparison) so a failed build retries on
/// the next attempt instead of being mistaken for "already up to date".
fn installed_head_marker(repo: &Path) -> PathBuf {
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

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("llm-hub/{VERSION}"))
        .build()
        .map_err(|e| e.to_string())
}

fn expected_asset_name() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let suffix = std::env::consts::EXE_SUFFIX;
    format!("llm-hub-{os}-{arch}{suffix}")
}

/// Writes the new binary next to the current one, then atomically renames
/// over it. The running process keeps executing the old inode.
#[cfg(not(windows))]
fn replace_binary(current: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = current.with_extension("update-tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write update failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }
    std::fs::rename(&tmp, current).map_err(|e| format!("replace binary failed: {e}"))
}

/// Windows refuses to overwrite a running executable but lets it be renamed,
/// so the running file moves aside before the new one takes its path.
#[cfg(windows)]
fn replace_binary(current: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = current.with_extension("update-tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write update failed: {e}"))?;
    let aside = move_aside(current)?;
    if let Err(e) = std::fs::rename(&tmp, current) {
        restore_set_aside(Some(&aside), current);
        return Err(format!("replace binary failed: {e}"));
    }
    Ok(())
}

/// Cargo cannot write `target/release/llm-hub.exe` on Windows while that file
/// runs, so a hub running its own build moves it aside first.
#[cfg(windows)]
fn set_aside_if_running_build(current: &Path, built: &Path) -> Result<Option<PathBuf>, String> {
    if !is_same_file(current, built) {
        return Ok(None);
    }
    move_aside(current).map(Some)
}

#[cfg(not(windows))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Matches the Windows variant, which can fail"
)]
fn set_aside_if_running_build(_current: &Path, _built: &Path) -> Result<Option<PathBuf>, String> {
    Ok(None)
}

#[cfg(windows)]
fn move_aside(running: &Path) -> Result<PathBuf, String> {
    let aside = running.with_extension("old.exe");
    if aside.exists() {
        std::fs::remove_file(&aside).map_err(|e| {
            format!(
                "cannot remove {} from the previous update (restart llm-hub, then retry): {e}",
                aside.display()
            )
        })?;
    }
    std::fs::rename(running, &aside)
        .map_err(|e| format!("cannot move the running binary aside: {e}"))?;
    Ok(aside)
}

/// Puts a set-aside binary back when nothing replaced it, so the path still runs.
fn restore_set_aside(aside: Option<&Path>, original: &Path) {
    let Some(aside) = aside else {
        return;
    };
    if original.exists() {
        return;
    }
    if let Err(e) = std::fs::rename(aside, original) {
        tracing::warn!(
            "cannot move {} back to {}: {e}",
            aside.display(),
            original.display()
        );
    }
}

/// The release this build is based on: `0.1.30+3.g842543c` is a local build on
/// top of release `0.1.30`, so the update check compares `0.1.30`.
fn release_version() -> &'static str {
    VERSION.split_once('+').map_or(VERSION, |(base, _)| base)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::FutureExt;
    use serde_json::json;
    use tokio::sync::Notify;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NEWER_TAG: &str = "v999.0.0";
    const HEAD: &str = "4b308469e56ff60351020aa8fcec2fcf6eae5387";

    struct FakeCommands {
        head: Option<&'static str>,
        status: Option<&'static str>,
        failing_program: Option<&'static str>,
        build: Option<&'static [u8]>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeCommands {
        fn clean() -> Self {
            Self {
                head: Some(HEAD),
                status: Some(""),
                failing_program: None,
                build: Some(b"fresh build"),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RepoCommands for FakeCommands {
        async fn run(
            &self,
            repo: &Path,
            program: &'static str,
            args: &'static [&'static str],
            failure_hint: &'static str,
        ) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            if self.failing_program == Some(program) {
                return Err(format!("{program} failed — {failure_hint}"));
            }
            if let ("cargo", Some(build)) = (program, self.build) {
                let release = repo.join("target").join("release");
                std::fs::create_dir_all(&release).unwrap();
                std::fs::write(release.join(binary_name()), build).unwrap();
            }
            Ok(())
        }

        async fn capture_git(
            &self,
            _repo: &Path,
            args: &'static [&'static str],
        ) -> Result<String, String> {
            let command = format!("git {}", args.join(" "));
            self.calls.lock().unwrap().push(command.clone());
            let reply = match args {
                ["rev-parse", "HEAD"] => self.head.map(|head| format!("{head}\n")),
                ["status", "--porcelain"] => self.status.map(str::to_string),
                _ => panic!("unexpected git call: {command}"),
            };
            reply.ok_or_else(|| format!("`{command}` failed"))
        }
    }

    fn supervised() -> bool {
        true
    }

    fn unsupervised() -> bool {
        false
    }

    fn updater(
        current: PathBuf,
        latest_release_url: String,
        commands: FakeCommands,
        is_supervised: fn() -> bool,
    ) -> Updater<FakeCommands> {
        Updater {
            current,
            latest_release_url,
            commands,
            is_supervised,
        }
    }

    fn installed_binary(dir: &Path, contents: &[u8]) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let current = bin.join(binary_name());
        std::fs::write(&current, contents).unwrap();
        current
    }

    fn source_checkout(dir: &Path) -> PathBuf {
        let repo = dir.join("llm-hub");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"llm-hub\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        repo
    }

    fn release_updater(
        dir: &Path,
        server: &MockServer,
        is_supervised: fn() -> bool,
    ) -> Updater<FakeCommands> {
        updater(
            installed_binary(dir, b"old binary"),
            format!("{}/releases/latest", server.uri()),
            FakeCommands::clean(),
            is_supervised,
        )
    }

    fn source_updater(
        repo: &Path,
        commands: FakeCommands,
        is_supervised: fn() -> bool,
    ) -> Updater<FakeCommands> {
        updater(
            installed_binary(repo, b"old binary"),
            String::new(),
            commands,
            is_supervised,
        )
    }

    async fn serve_release(server: &MockServer, tag: &str, asset_name: &str) {
        Mock::given(method("GET"))
            .and(path("/releases/latest"))
            .and(header("user-agent", format!("llm-hub/{VERSION}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tag_name": tag,
                "assets": [{
                    "name": asset_name,
                    "browser_download_url": format!("{}/download/asset", server.uri()),
                }],
            })))
            .mount(server)
            .await;
    }

    async fn serve_asset(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/download/asset"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn current_tag() -> String {
        format!("v{}", release_version())
    }

    fn restarted(notify: &Notify) -> bool {
        notify.notified().now_or_never().is_some()
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    #[tokio::test]
    async fn a_release_matching_the_running_version_is_already_up_to_date() {
        let server = MockServer::start().await;
        serve_release(&server, &current_tag(), &expected_asset_name()).await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(outcome, Ok(UpdateOutcome::AlreadyUpToDate));
        assert_eq!(read(&updater.current), b"old binary");
    }

    #[tokio::test]
    async fn a_newer_release_replaces_the_binary_with_its_asset() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        serve_asset(
            &server,
            ResponseTemplate::new(200).set_body_bytes(b"new binary".to_vec()),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(outcome, Ok(UpdateOutcome::Applied));
        assert_eq!(read(&updater.current), b"new binary");
        assert!(!updater.current.with_extension("update-tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&updater.current)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[tokio::test]
    async fn a_newer_release_without_an_asset_for_this_platform_is_an_error() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, "llm-hub-plan9-mips").await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(
            outcome,
            Err(format!(
                "no release asset named {} in v999.0.0",
                expected_asset_name()
            ))
        );
        assert_eq!(read(&updater.current), b"old binary");
    }

    #[tokio::test]
    async fn a_failed_asset_download_is_an_error() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        serve_asset(&server, ResponseTemplate::new(404)).await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert!(error.contains("404"), "{error}");
        assert_eq!(read(&updater.current), b"old binary");
    }

    #[tokio::test]
    async fn a_failing_release_api_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases/latest"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert!(error.contains("500"), "{error}");
    }

    #[tokio::test]
    async fn a_malformed_release_response_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"assets": []})))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);

        let outcome = updater.self_update().await;

        assert!(outcome.is_err());
    }

    #[tokio::test]
    async fn an_auto_update_applies_a_newer_release_and_restarts_a_supervised_hub() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        serve_asset(
            &server,
            ResponseTemplate::new(200).set_body_bytes(b"new binary".to_vec()),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"new binary");
        assert!(restarted(&notify));
    }

    #[tokio::test]
    async fn an_unsupervised_hub_applies_the_auto_update_without_restarting() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        serve_asset(
            &server,
            ResponseTemplate::new(200).set_body_bytes(b"new binary".to_vec()),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, unsupervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"new binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn with_auto_update_disabled_a_newer_release_is_only_reported() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        Mock::given(path("/download/asset"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(false, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn an_up_to_date_hub_skips_the_auto_update() {
        let server = MockServer::start().await;
        serve_release(&server, &current_tag(), &expected_asset_name()).await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_failing_update_check_changes_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_failed_auto_update_does_not_restart() {
        let server = MockServer::start().await;
        serve_release(&server, NEWER_TAG, &expected_asset_name()).await;
        serve_asset(&server, ResponseTemplate::new(500)).await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_release_pulled_between_check_and_update_does_not_restart() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"tag_name": NEWER_TAG, "assets": []})),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        serve_release(&server, &current_tag(), &expected_asset_name()).await;
        let dir = tempfile::tempdir().unwrap();
        let updater = release_updater(dir.path(), &server, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_source_install_updates_by_pulling_and_rebuilding() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let updater = source_updater(&repo, FakeCommands::clean(), unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(outcome, Ok(UpdateOutcome::Applied));
        assert_eq!(
            updater.commands.calls(),
            [
                "git pull --ff-only",
                "git rev-parse HEAD",
                "cargo build --release"
            ]
        );
        assert_eq!(read(&updater.current), b"fresh build");
        assert_eq!(installed_head(&repo).as_deref(), Some(HEAD));
    }

    #[test]
    fn a_source_rebuild_keeps_the_mistralrs_feature_when_the_binary_has_it() {
        assert_eq!(
            cargo_build_args_for(true),
            ["build", "--release", "--features", "mistralrs"]
        );
        assert_eq!(cargo_build_args_for(false), ["build", "--release"]);
    }

    #[tokio::test]
    async fn a_source_install_built_from_head_is_already_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(installed_head_marker(&repo), format!("{HEAD}\n")).unwrap();
        let updater = source_updater(&repo, FakeCommands::clean(), unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(outcome, Ok(UpdateOutcome::AlreadyUpToDate));
        assert_eq!(
            updater.commands.calls(),
            ["git pull --ff-only", "git rev-parse HEAD"]
        );
        assert_eq!(read(&updater.current), b"old binary");
    }

    #[tokio::test]
    async fn a_failed_pull_stops_the_source_update() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            failing_program: Some("git"),
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert_eq!(error, format!("git failed — {PULL_HINT}"));
        assert_eq!(updater.commands.calls(), ["git pull --ff-only"]);
    }

    #[tokio::test]
    async fn an_unreadable_head_stops_the_source_update() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            head: None,
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert_eq!(error, "`git rev-parse HEAD` failed");
        assert_eq!(read(&updater.current), b"old binary");
    }

    #[tokio::test]
    async fn a_failed_build_keeps_the_installed_binary_and_retries_later() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            failing_program: Some("cargo"),
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert_eq!(error, format!("cargo failed — {BUILD_HINT}"));
        assert_eq!(read(&updater.current), b"old binary");
        assert_eq!(installed_head(&repo), None);
    }

    #[tokio::test]
    async fn a_build_that_leaves_no_binary_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            build: None,
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, unsupervised);

        let error = updater.self_update().await.unwrap_err();

        assert!(error.starts_with("read fresh build "), "{error}");
        assert_eq!(read(&updater.current), b"old binary");
        assert_eq!(installed_head(&repo), None);
    }

    #[tokio::test]
    async fn a_hub_running_its_own_build_keeps_the_fresh_build_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let release = repo.join("target").join("release");
        std::fs::create_dir_all(&release).unwrap();
        let current = release.join(binary_name());
        std::fs::write(&current, b"old binary").unwrap();
        let updater = updater(current, String::new(), FakeCommands::clean(), unsupervised);

        let outcome = updater.self_update().await;

        assert_eq!(outcome, Ok(UpdateOutcome::Applied));
        assert_eq!(read(&updater.current), b"fresh build");
        assert!(!updater.current.with_extension("update-tmp").exists());
        assert_eq!(installed_head(&repo).as_deref(), Some(HEAD));
    }

    #[tokio::test]
    async fn with_auto_update_disabled_a_source_install_runs_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let updater = source_updater(&repo, FakeCommands::clean(), supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(false, &notify).await;

        assert!(updater.commands.calls().is_empty());
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_dirty_checkout_is_not_auto_updated() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            status: Some(" M src/main.rs\n"),
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(updater.commands.calls(), ["git status --porcelain"]);
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn an_unreadable_worktree_status_skips_the_source_auto_update() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            status: None,
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(updater.commands.calls(), ["git status --porcelain"]);
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn a_clean_checkout_auto_updates_and_restarts_a_supervised_hub() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let updater = source_updater(&repo, FakeCommands::clean(), supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"fresh build");
        assert!(restarted(&notify));
    }

    #[tokio::test]
    async fn a_failed_source_auto_update_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        let commands = FakeCommands {
            failing_program: Some("cargo"),
            ..FakeCommands::clean()
        };
        let updater = source_updater(&repo, commands, supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert_eq!(read(&updater.current), b"old binary");
        assert!(!restarted(&notify));
    }

    #[tokio::test]
    async fn an_up_to_date_source_checkout_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let repo = source_checkout(dir.path());
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(installed_head_marker(&repo), HEAD).unwrap();
        let updater = source_updater(&repo, FakeCommands::clean(), supervised);
        let notify = Notify::new();

        updater.check_and_maybe_update(true, &notify).await;

        assert!(
            !updater
                .commands
                .calls()
                .contains(&"cargo build --release".to_string())
        );
        assert!(!restarted(&notify));
    }

    #[test]
    fn the_system_updater_reads_the_latest_github_release() {
        let updater = Updater::system(PathBuf::from("/opt/llm-hub/llm-hub"));

        assert_eq!(
            updater.latest_release_url,
            format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest")
        );
        assert_eq!(updater.current, PathBuf::from("/opt/llm-hub/llm-hub"));
    }

    #[test]
    fn the_release_version_drops_local_build_metadata() {
        let version = release_version();

        assert!(!version.contains('+'));
        assert!(VERSION.starts_with(version));
    }

    #[test]
    fn the_release_asset_is_named_after_this_platform() {
        let name = expected_asset_name();

        assert!(name.starts_with(&format!(
            "llm-hub-{}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )));
    }

    #[test]
    fn recording_the_installed_head_without_a_target_dir_is_skipped() {
        let dir = tempfile::tempdir().unwrap();

        record_installed_head(dir.path(), HEAD);

        assert_eq!(installed_head(dir.path()), None);
    }

    #[test]
    fn only_the_same_existing_file_counts_as_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        assert!(is_same_file(&a, &dir.path().join(".").join("a")));
        assert!(!is_same_file(&a, &b));
        assert!(!is_same_file(&a, &dir.path().join("missing")));
    }

    #[test]
    fn replacing_a_binary_in_a_missing_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("missing").join(binary_name());

        let error = replace_binary(&current, b"new binary").unwrap_err();

        assert!(error.starts_with("write update failed: "), "{error}");
    }

    #[test]
    fn a_set_aside_binary_is_restored_when_nothing_replaced_it() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("llm-hub");
        let aside = dir.path().join("llm-hub.old");
        std::fs::write(&aside, b"old binary").unwrap();

        restore_set_aside(Some(&aside), &original);

        assert_eq!(read(&original), b"old binary");
        assert!(!aside.exists());
    }

    #[test]
    fn a_set_aside_binary_stays_aside_when_a_new_one_took_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("llm-hub");
        let aside = dir.path().join("llm-hub.old");
        std::fs::write(&original, b"new binary").unwrap();
        std::fs::write(&aside, b"old binary").unwrap();

        restore_set_aside(Some(&aside), &original);

        assert_eq!(read(&original), b"new binary");
        assert_eq!(read(&aside), b"old binary");
    }

    #[test]
    fn a_missing_set_aside_binary_leaves_the_path_empty() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("llm-hub");

        restore_set_aside(Some(&dir.path().join("llm-hub.old")), &original);
        restore_set_aside(None, &original);

        assert!(!original.exists());
    }

    #[tokio::test]
    async fn system_commands_run_and_read_git_in_the_checkout() {
        let dir = tempfile::tempdir().unwrap();

        SystemCommands
            .run(dir.path(), "git", &["init", "--quiet"], PULL_HINT)
            .await
            .unwrap();
        let clean = SystemCommands
            .capture_git(dir.path(), &["status", "--porcelain"])
            .await;
        std::fs::write(dir.path().join("new-file"), b"x").unwrap();
        let dirty = SystemCommands
            .capture_git(dir.path(), &["status", "--porcelain"])
            .await;

        assert_eq!(clean, Ok(String::new()));
        assert_eq!(dirty, Ok("?? new-file\n".to_string()));
    }

    #[tokio::test]
    async fn a_failing_git_read_reports_the_command_and_checkout() {
        let dir = tempfile::tempdir().unwrap();

        let error = SystemCommands
            .capture_git(
                dir.path(),
                &["rev-parse", "--verify", "llm-hub-missing-ref"],
            )
            .await
            .unwrap_err();

        assert!(
            error.starts_with("`git rev-parse --verify llm-hub-missing-ref` failed ("),
            "{error}"
        );
        assert!(
            error.ends_with(&format!("in {}", dir.path().display())),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_failing_command_reports_the_hint() {
        let dir = tempfile::tempdir().unwrap();

        let error = SystemCommands
            .run(
                dir.path(),
                "git",
                &["rev-parse", "--verify", "--quiet", "llm-hub-missing-ref"],
                BUILD_HINT,
            )
            .await
            .unwrap_err();

        assert!(
            error.starts_with("`git rev-parse --verify --quiet llm-hub-missing-ref` failed ("),
            "{error}"
        );
        assert!(error.ends_with(BUILD_HINT), "{error}");
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_as_not_found() {
        let dir = tempfile::tempdir().unwrap();

        let error = SystemCommands
            .run(dir.path(), "llm-hub-missing-tool", &[], BUILD_HINT)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "llm-hub-missing-tool not found on PATH — required to update a source install"
        );
    }
}

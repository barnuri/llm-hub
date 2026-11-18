use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// Delay before firing the shutdown notify so the HTTP response that
/// requested the restart can flush to the client first.
const RESPONSE_FLUSH_DELAY: Duration = Duration::from_millis(300);

static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True when a supervisor (launchd / systemd / Task Scheduler) will bring the
/// process back after exit — `llm-hub service install` sets `LLM_HUB_SERVICE=1`.
pub fn is_supervised() -> bool {
    service_flag_set(std::env::var("LLM_HUB_SERVICE").ok().as_deref())
}

fn service_flag_set(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Requests a graceful restart: flags the post-drain respawn and fires the
/// shutdown notify shortly after, letting in-flight responses complete.
pub fn request_restart(restart_notify: Arc<Notify>) {
    flag_restart(&RESTART_REQUESTED, restart_notify);
}

fn flag_restart(requested: &AtomicBool, restart_notify: Arc<Notify>) {
    requested.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        tokio::time::sleep(RESPONSE_FLUSH_DELAY).await;
        restart_notify.notify_one();
    });
}

/// After the server drains: respawn the current executable (same args) when
/// no supervisor will do it for us. Returns whether a child was spawned.
///
/// # Errors
/// Fails when the current executable path cannot be resolved or the spawn fails.
pub fn respawn_if_requested() -> Result<bool, String> {
    respawn(
        RESTART_REQUESTED.load(Ordering::SeqCst),
        is_supervised(),
        crate::utils::exe::current_exe_resolved,
        spawn_detached,
    )
}

fn respawn(
    requested: bool,
    supervised: bool,
    resolve_exe: impl FnOnce() -> std::io::Result<PathBuf>,
    spawn: impl FnOnce(&Path, Vec<String>) -> std::io::Result<()>,
) -> Result<bool, String> {
    if !requested || supervised {
        return Ok(false);
    }
    let exe = resolve_exe().map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    spawn(&exe, args).map_err(|e| format!("respawn failed: {e}"))?;
    Ok(true)
}

fn spawn_detached(exe: &Path, args: Vec<String>) -> std::io::Result<()> {
    Command::new(exe).args(args).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn unexpected_resolve() -> std::io::Result<PathBuf> {
        panic!("the exe must not be resolved")
    }

    fn unexpected_spawn(_: &Path, _: Vec<String>) -> std::io::Result<()> {
        panic!("nothing must be spawned")
    }

    #[test]
    fn respawn_without_request_is_noop() {
        assert_eq!(respawn_if_requested(), Ok(false));
    }

    #[test]
    fn only_a_service_flag_of_one_means_supervised() {
        assert!(service_flag_set(Some("1")));
        assert!(!service_flag_set(Some("0")));
        assert!(!service_flag_set(Some("")));
        assert!(!service_flag_set(None));
    }

    #[test]
    fn an_unrequested_restart_spawns_nothing() {
        let result = respawn(false, false, unexpected_resolve, unexpected_spawn);

        assert_eq!(result, Ok(false));
    }

    #[test]
    fn a_supervised_restart_leaves_the_respawn_to_the_supervisor() {
        let result = respawn(true, true, unexpected_resolve, unexpected_spawn);

        assert_eq!(result, Ok(false));
    }

    #[test]
    fn a_requested_restart_respawns_the_resolved_exe_with_the_current_args() {
        let spawned = RefCell::new(None);

        let result = respawn(
            true,
            false,
            || Ok(PathBuf::from("/opt/llm-hub/llm-hub")),
            |exe, args| {
                spawned.replace(Some((exe.to_path_buf(), args)));
                Ok(())
            },
        );

        assert_eq!(result, Ok(true));
        let expected_args: Vec<String> = std::env::args().skip(1).collect();
        assert_eq!(
            spawned.into_inner(),
            Some((PathBuf::from("/opt/llm-hub/llm-hub"), expected_args))
        );
    }

    #[test]
    fn an_unresolvable_exe_is_an_error() {
        let result = respawn(
            true,
            false,
            || Err(std::io::Error::other("no exe")),
            unexpected_spawn,
        );

        assert_eq!(
            result,
            Err("cannot resolve current exe: no exe".to_string())
        );
    }

    #[test]
    fn a_failed_spawn_is_an_error() {
        let result = respawn(
            true,
            false,
            || Ok(PathBuf::from("/opt/llm-hub/llm-hub")),
            |_, _| Err(std::io::Error::other("spawn refused")),
        );

        assert_eq!(result, Err("respawn failed: spawn refused".to_string()));
    }

    #[test]
    fn spawning_a_missing_exe_fails() {
        let dir = tempfile::tempdir().unwrap();

        let result = spawn_detached(&dir.path().join("missing-llm-hub"), Vec::new());

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn a_restart_request_flags_the_respawn_and_notifies_after_the_flush_delay() {
        let requested = AtomicBool::new(false);
        let notify = Arc::new(Notify::new());
        let started = tokio::time::Instant::now();

        flag_restart(&requested, notify.clone());
        let notified = tokio::time::timeout(Duration::from_secs(10), notify.notified()).await;

        assert!(notified.is_ok());
        assert!(requested.load(Ordering::SeqCst));
        assert!(started.elapsed() >= RESPONSE_FLUSH_DELAY);
    }
}

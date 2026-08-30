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
    std::env::var("LLM_HUB_SERVICE").as_deref() == Ok("1")
}

/// Requests a graceful restart: flags the post-drain respawn and fires the
/// shutdown notify shortly after, letting in-flight responses complete.
pub fn request_restart(restart_notify: Arc<Notify>) {
    RESTART_REQUESTED.store(true, Ordering::SeqCst);
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
    if !RESTART_REQUESTED.load(Ordering::SeqCst) || is_supervised() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve current exe: {e}"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Command::new(exe)
        .args(args)
        .spawn()
        .map_err(|e| format!("respawn failed: {e}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respawn_without_request_is_noop() {
        assert_eq!(respawn_if_requested(), Ok(false));
    }
}

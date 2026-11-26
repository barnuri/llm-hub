//! Single-instance guard, PID file and the zero-gap handover on restart.
//!
//! A restart starts a successor that binds the same address with `SO_REUSEPORT`. The successor
//! writes its PID file once it listens; the old process then drains and exits. The successor waits
//! for the old process to exit before it starts local model processes, so the old process stops
//! only its own children. `llm-hub service handover` uses the same steps for a deploy.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::net::{TcpListener, TcpSocket};

/// Env var that marks a successor and carries the PID of the process it replaces.
pub const HANDOVER_VAR: &str = "LLM_HUB_HANDOVER_FROM";
/// How long the old process waits for the successor to listen before it keeps serving.
const READY_TIMEOUT: Duration = Duration::from_mins(1);
/// How long a successor waits for the old process to exit before it starts local models.
const OLD_EXIT_TIMEOUT: Duration = Duration::from_mins(2);
const POLL: Duration = Duration::from_millis(200);

static BIND: OnceLock<String> = OnceLock::new();

/// Records the address this process serves, for the restart handover.
pub fn set_bind(bind: &str) {
    let _ = BIND.set(bind.to_owned());
}

/// The address this process serves, if it has started serving.
pub fn bind() -> Option<&'static str> {
    BIND.get().map(String::as_str)
}

/// The PID of the process this one replaces, when this process is a handover successor.
pub fn handover_from() -> Option<u32> {
    std::env::var(HANDOVER_VAR).ok()?.parse().ok()
}

/// The PID file for `bind`, in the temp dir (one file per address).
pub fn pid_file(bind: &str) -> PathBuf {
    let name: String = bind
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("llm-hub-{name}.pid"))
}

/// Writes this process's PID to the PID file of `bind`.
///
/// # Errors
/// Fails when the file cannot be written.
pub fn write_pid(bind: &str) -> std::io::Result<()> {
    record_pid(bind, std::process::id())
}

/// Writes `pid` to the PID file of `bind`, for example to give it back after a failed handover.
///
/// # Errors
/// Fails when the file cannot be written.
pub fn record_pid(bind: &str, pid: u32) -> std::io::Result<()> {
    std::fs::write(pid_file(bind), format!("{pid}\n"))
}

/// The PID in the PID file of `bind`, if the file holds one.
pub fn read_pid(bind: &str) -> Option<u32> {
    std::fs::read_to_string(pid_file(bind))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whether process `pid` exists. A process owned by another user still counts as existing.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // Pid 0 names the caller's process group, so kill(0, 0) would always succeed.
    if pid == 0 {
        return false;
    }
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether process `pid` exists. Windows has no signal 0, so the check reports no process.
#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// The PID of a live hub other than this process, serving `bind`.
pub fn live_server(bind: &str) -> Option<u32> {
    let pid = read_pid(bind)?;
    (pid != std::process::id() && pid_alive(pid)).then_some(pid)
}

/// Waits while a live hub serves `bind`. Under a supervisor, the caller exits after this returns,
/// so the supervisor starts a fresh process that serves once the live one is gone.
pub async fn wait_while_served(bind: &str) {
    while live_server(bind).is_some() {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Waits until process `pid` exits, or until the timeout passes.
pub async fn wait_gone(pid: u32) {
    let deadline = tokio::time::Instant::now() + OLD_EXIT_TIMEOUT;
    while pid_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL).await;
    }
}

/// Binds `bind` with `SO_REUSEPORT`, so a successor can listen beside the old process.
///
/// # Errors
/// Fails when the address cannot be bound.
pub async fn bind_reuseport(bind: &str) -> std::io::Result<TcpListener> {
    let Ok(addr) = bind.parse::<SocketAddr>() else {
        return TcpListener::bind(bind).await;
    };
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    #[cfg(unix)]
    socket.set_reuseport(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Starts `exe` with `args` as the successor of this process and waits until it writes its PID.
///
/// # Errors
/// Fails when the successor cannot start, exits first, or is not ready within the timeout. The
/// successor is terminated on timeout.
pub async fn spawn_successor(bind: &str, exe: &Path, args: Vec<String>) -> Result<u32, String> {
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .env(HANDOVER_VAR, std::process::id().to_string());
    start_successor(bind, command).await.map(|child| child.id())
}

/// Runs `command` as a successor and waits until it writes its PID to the PID file of `bind`.
/// Returns the running child, so the caller can tell when it exits.
///
/// # Errors
/// Fails when the successor cannot start, exits first, or is not ready within the timeout. The
/// successor is terminated on timeout.
pub async fn start_successor(
    bind: &str,
    mut command: std::process::Command,
) -> Result<std::process::Child, String> {
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start the successor: {e}"))?;
    let child_pid = child.id();
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if read_pid(bind) == Some(child_pid) {
            return Ok(child);
        }
        // try_wait reaps an exited successor; a zombie would still look alive to pid_alive.
        if let Ok(Some(_)) = child.try_wait() {
            return Err("the successor exited before it listened".to_owned());
        }
        if tokio::time::Instant::now() >= deadline {
            terminate(child_pid);
            return Err(format!(
                "the successor did not listen within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Sends SIGTERM to `pid`. A hub drains its open requests on SIGTERM, then exits.
#[cfg(unix)]
pub fn terminate(pid: u32) {
    // Pid 0 names the caller's process group, so it never reaches kill.
    if pid == 0 {
        return;
    }
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: sends SIGTERM to one process id; the callers pass a hub or a successor they own.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// Windows has no SIGTERM, so this does nothing there.
#[cfg(not(unix))]
pub fn terminate(_pid: u32) {}

/// Parent-exit watch for the server. A successor skips it: its parent is the process it replaces,
/// which exits on purpose after the handover.
pub async fn parent_exit(vars: &std::collections::HashMap<String, String>) {
    if handover_from().is_some() {
        std::future::pending::<()>().await;
    }
    crate::services::parent_watch::wait_for_parent_exit(vars).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_names_each_address_apart() {
        let a = pid_file("0.0.0.0:8888");
        let b = pid_file("0.0.0.0:8889");
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with("llm-hub-0_0_0_0_8888.pid"));
    }

    #[test]
    fn own_pid_is_not_a_live_server() {
        let bind = "127.0.0.1:1";
        write_pid(bind).expect("write the pid file");
        assert_eq!(read_pid(bind), Some(std::process::id()));
        assert_eq!(live_server(bind), None);
        let _ = std::fs::remove_file(pid_file(bind));
    }

    #[test]
    fn a_dead_pid_is_not_a_live_server() {
        let bind = "127.0.0.1:2";
        std::fs::write(pid_file(bind), "999999999\n").expect("write the pid file");
        assert_eq!(live_server(bind), None);
        let _ = std::fs::remove_file(pid_file(bind));
    }

    #[cfg(unix)]
    #[test]
    fn this_process_is_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_successor_that_exits_before_listening_is_an_error() {
        let result = spawn_successor("127.0.0.1:3", Path::new("/usr/bin/true"), Vec::new()).await;

        assert_eq!(
            result,
            Err("the successor exited before it listened".to_owned())
        );
    }

    #[tokio::test]
    async fn a_successor_that_cannot_start_is_an_error() {
        let result =
            spawn_successor("127.0.0.1:4", Path::new("/no/such/llm-hub"), Vec::new()).await;

        assert!(
            result
                .unwrap_err()
                .starts_with("cannot start the successor:")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_successor_that_writes_its_pid_is_returned_running() {
        let bind = "127.0.0.1:6";
        let file = pid_file(bind);
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exec sleep 30", file.display()));

        let mut child = start_successor(bind, command).await.expect("ready");

        assert_eq!(read_pid(bind), Some(child.id()));
        assert!(child.try_wait().expect("status").is_none());
        terminate(child.id());
        assert!(child.wait().is_ok());
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn a_recorded_pid_is_read_back() {
        let bind = "127.0.0.1:7";
        record_pid(bind, 4242).expect("write the pid file");
        assert_eq!(read_pid(bind), Some(4242));
        let _ = std::fs::remove_file(pid_file(bind));
    }

    #[cfg(unix)]
    #[test]
    fn terminating_pid_zero_signals_nothing() {
        terminate(0);
        assert!(pid_alive(std::process::id()));
    }

    #[tokio::test]
    async fn waiting_for_a_missing_process_returns_at_once() {
        let started = tokio::time::Instant::now();

        wait_gone(999_999_999).await;

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn two_listeners_share_one_port() {
        let first = bind_reuseport("127.0.0.1:0").await.expect("first bind");
        let port = first.local_addr().expect("address").port();
        let second = bind_reuseport(&format!("127.0.0.1:{port}")).await;
        assert!(
            second.is_ok(),
            "the second listener must bind the same port: {second:?}"
        );
    }
}

//! Single-instance guard, PID file and the zero-gap handover on restart.
//!
//! A restart starts a successor that binds the same address with `SO_REUSEPORT`. The successor
//! writes its PID file once it listens; the old process then drains and exits. The successor waits
//! for the old process to exit before it starts local model processes, so the old process stops
//! only its own children. `llm-hub service handover` uses the same steps for a deploy.
//!
//! A deploy successor runs outside the supervisor, so it is a bridge: it writes a bridge marker.
//! When the old process exits, the supervisor starts a new process. That process finds the
//! marker and takes the port over from the bridge with the same steps, so the supervisor owns
//! the hub that serves and no second copy waits beside it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::net::{TcpListener, TcpSocket};

/// Env var that marks a successor and carries the PID of the process it replaces.
pub const HANDOVER_VAR: &str = "LLM_HUB_HANDOVER_FROM";
/// Env var that marks a bridge: a successor that `service handover` starts outside the
/// supervisor. The supervisor's next process takes the port over from it.
pub const BRIDGE_VAR: &str = "LLM_HUB_HANDOVER_BRIDGE";
/// How long the old process waits for the successor to listen before it keeps serving.
const READY_TIMEOUT: Duration = Duration::from_mins(1);
/// How long the old process keeps its listener after a successor listens. Connections that
/// already wait in its accept queue are served first, and the kernel sends new ones to the
/// successor. Closing the listener at once resets those queued connections.
#[cfg(not(test))]
pub const SETTLE: Duration = Duration::from_secs(1);
/// Unit tests wait on this, so it is short there.
#[cfg(test)]
pub const SETTLE: Duration = Duration::from_millis(10);
/// How long a successor waits for the old process to exit before it starts local models.
const OLD_EXIT_TIMEOUT: Duration = Duration::from_mins(2);
/// How often a handover polls the PID file and the old process.
#[cfg(not(test))]
const POLL: Duration = Duration::from_millis(200);
/// Unit tests wait on real processes through this poll, so it is short there.
#[cfg(test)]
const POLL: Duration = Duration::from_millis(20);

static BIND: OnceLock<String> = OnceLock::new();
/// The bridge this process takes the port over from, when the supervisor started it beside one.
static TAKE_OVER_FROM: OnceLock<u32> = OnceLock::new();

/// What a starting hub does about the hub that already serves its address.
#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    /// Nothing else serves the address, or this process is a successor: serve.
    Serve,
    /// A bridge serves the address and this process runs under the supervisor: replace the bridge.
    TakeOver(u32),
    /// Another hub serves the address under the supervisor: wait until it exits.
    Wait,
    /// Another hub serves the address and no supervisor runs this process: refuse to start.
    Refuse(u32),
}

/// Decides how this process starts on `bind`.
pub fn startup(bind: &str) -> Startup {
    decide_startup(
        handover_from().is_some(),
        live_server(bind),
        bridge_pid(bind),
        crate::services::restart::is_supervised(),
    )
}

fn decide_startup(
    successor: bool,
    live: Option<u32>,
    bridge: Option<u32>,
    supervised: bool,
) -> Startup {
    let Some(pid) = live.filter(|_| !successor) else {
        return Startup::Serve;
    };
    if !supervised {
        return Startup::Refuse(pid);
    }
    if bridge == Some(pid) {
        Startup::TakeOver(pid)
    } else {
        Startup::Wait
    }
}

/// Makes this process the successor of the bridge `pid`, as if the bridge had started it.
pub fn take_over(pid: u32) {
    let _ = TAKE_OVER_FROM.set(pid);
}

/// The bridge this process takes over from, if any.
pub fn taking_over() -> Option<u32> {
    TAKE_OVER_FROM.get().copied()
}

/// Whether `service handover` started this process as a bridge.
fn is_bridge() -> bool {
    bridge_flag_set(std::env::var(BRIDGE_VAR).ok().as_deref())
}

fn bridge_flag_set(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The bridge marker file for `bind`: it names the bridge that serves the address.
pub fn bridge_file(bind: &str) -> PathBuf {
    pid_file(bind).with_extension("bridge")
}

/// The PID in the bridge marker of `bind`, if the marker holds one.
pub fn bridge_pid(bind: &str) -> Option<u32> {
    read_number(&bridge_file(bind))
}

/// Writes this process's PID to the bridge marker of `bind` when it runs as a bridge. It runs
/// before [`write_pid`], so a process that reads the PID file of a bridge also finds the marker.
///
/// # Errors
/// Fails when the marker cannot be written.
pub fn mark_bridge(bind: &str) -> std::io::Result<()> {
    if !is_bridge() {
        return Ok(());
    }
    std::fs::write(bridge_file(bind), format!("{}\n", std::process::id()))
}

/// Ends a take-over once this process listens: removes the bridge marker, lets the bridge empty
/// its accept queue and sends it SIGTERM, so it drains its open requests and exits.
pub async fn finish_take_over(bind: &str, bridge: u32) {
    let _ = std::fs::remove_file(bridge_file(bind));
    tokio::time::sleep(SETTLE).await;
    terminate(bridge);
}

/// Records the address this process serves, for the restart handover.
pub fn set_bind(bind: &str) {
    let _ = BIND.set(bind.to_owned());
}

/// The address this process serves, if it has started serving.
pub fn bind() -> Option<&'static str> {
    BIND.get().map(String::as_str)
}

/// The PID of the process this one replaces: the process that started it as a handover
/// successor, or the bridge it takes the port over from.
pub fn handover_from() -> Option<u32> {
    taking_over().or_else(|| std::env::var(HANDOVER_VAR).ok()?.parse().ok())
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
    read_number(&pid_file(bind))
}

fn read_number(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
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

    #[test]
    fn a_free_address_is_served() {
        assert_eq!(decide_startup(false, None, None, true), Startup::Serve);
        assert_eq!(decide_startup(false, None, Some(5), false), Startup::Serve);
    }

    #[test]
    fn a_successor_serves_beside_the_live_hub() {
        assert_eq!(decide_startup(true, Some(5), None, true), Startup::Serve);
        assert_eq!(
            decide_startup(true, Some(5), Some(5), false),
            Startup::Serve
        );
    }

    #[test]
    fn the_supervised_process_takes_over_from_a_live_bridge() {
        assert_eq!(
            decide_startup(false, Some(5), Some(5), true),
            Startup::TakeOver(5)
        );
    }

    #[test]
    fn the_supervised_process_waits_for_a_hub_that_is_not_the_bridge() {
        assert_eq!(decide_startup(false, Some(5), None, true), Startup::Wait);
        assert_eq!(decide_startup(false, Some(5), Some(6), true), Startup::Wait);
    }

    #[test]
    fn an_unsupervised_process_refuses_a_busy_address() {
        assert_eq!(
            decide_startup(false, Some(5), Some(5), false),
            Startup::Refuse(5)
        );
        assert_eq!(
            decide_startup(false, Some(5), None, false),
            Startup::Refuse(5)
        );
    }

    #[test]
    fn an_address_nobody_serves_starts_serving() {
        let bind = "127.0.0.1:8";
        let _ = std::fs::remove_file(pid_file(bind));

        assert_eq!(startup(bind), Startup::Serve);
    }

    #[test]
    fn only_a_bridge_flag_of_one_marks_a_bridge() {
        assert!(bridge_flag_set(Some("1")));
        assert!(!bridge_flag_set(Some("0")));
        assert!(!bridge_flag_set(None));
        assert!(!is_bridge());
    }

    #[test]
    fn the_bridge_marker_sits_beside_the_pid_file() {
        let bind = "0.0.0.0:8888";

        let marker = bridge_file(bind);

        assert!(
            marker
                .to_string_lossy()
                .ends_with("llm-hub-0_0_0_0_8888.bridge")
        );
        assert_eq!(marker.parent(), pid_file(bind).parent());
    }

    #[test]
    fn a_process_that_is_not_a_bridge_writes_no_marker() {
        let bind = "127.0.0.1:9";
        let _ = std::fs::remove_file(bridge_file(bind));

        mark_bridge(bind).expect("no marker to write");

        assert_eq!(bridge_pid(bind), None);
        assert!(!bridge_file(bind).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finishing_a_take_over_removes_the_marker_and_stops_the_bridge() {
        let bind = "127.0.0.1:10";
        let mut bridge = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("start the bridge");
        std::fs::write(bridge_file(bind), format!("{}\n", bridge.id())).expect("marker");
        assert_eq!(bridge_pid(bind), Some(bridge.id()));

        finish_take_over(bind, bridge.id()).await;

        assert!(!bridge_file(bind).exists());
        let status = bridge.wait().expect("bridge status");
        assert!(!status.success(), "the bridge must stop on SIGTERM");
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

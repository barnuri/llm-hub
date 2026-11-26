use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::process::{Child, Command};

use crate::configs::HubTimings;
use crate::configs::{HubConfig, ManagedCommand, ProfileConfig};
use crate::consts::{MANAGED_READY_POLL_INTERVAL, MANAGED_STOP_GRACE};

/// Starts each profile's local server on demand and stops it when idle.
pub struct ProcessManager {
    entries: DashMap<String, Arc<ManagedEntry>>,
    logs_dir: PathBuf,
    timings: HubTimings,
}

/// One profile's process. The async mutex serialises start and stop, so
/// concurrent requests share a single start.
struct ManagedEntry {
    profile: String,
    command: ManagedCommand,
    child: tokio::sync::Mutex<Option<Child>>,
    last_used: Mutex<Instant>,
    // Backoff state to avoid rapid respawn attempts when a process repeatedly fails.
    backoff_until: Mutex<Option<Instant>>,
    backoff_secs: Mutex<u64>,
    // Last observed exit reason (if any) to help operators diagnose failures.
    last_exit_reason: Mutex<Option<String>>,
    stop_grace: Duration,
}

impl ProcessManager {
    /// Creates a manager that writes each process's output to `<logs_dir>/<profile>.log`.
    pub fn new(logs_dir: impl Into<PathBuf>, timings: HubTimings) -> ProcessManager {
        ProcessManager {
            entries: DashMap::new(),
            logs_dir: logs_dir.into(),
            timings,
        }
    }

    /// Makes sure the profile's server accepts connections. A profile without
    /// a command, or a port another process already serves, needs nothing.
    ///
    /// # Errors
    /// A message naming the profile and its log file when the process cannot
    /// be spawned, exits early, or does not listen within the start timeout.
    /// After a failed start, requests fail fast with the last reason until a
    /// backoff (5s doubling to 60s) passes, so a broken command is not respawned per request.
    pub async fn ensure_running(&self, profile: &ProfileConfig) -> Result<(), String> {
        let Some(command) = &profile.managed else {
            return Ok(());
        };
        let entry = self.entry_for(&profile.name, command);
        entry.touch();
        let mut child = entry.child.lock().await;
        if child.as_mut().is_some_and(is_alive) {
            return Ok(());
        }
        if let Some(reason) = entry.backoff_reason() {
            return Err(reason);
        }
        *child = None;
        if accepts_connections(&command.address).await {
            entry.reset_backoff();
            return Ok(());
        }
        let log_path = self.log_path(&profile.name);
        match start(&profile.name, command, &log_path).await {
            Ok(started) => {
                *child = Some(started);
                entry.reset_backoff();
                entry.touch();
                Ok(())
            }
            Err(reason) => {
                let message = format!(
                    "managed process for profile {} did not start: {reason}; see {}",
                    profile.name,
                    log_path.display()
                );
                entry.record_exit_reason(message.clone());
                Err(message)
            }
        }
    }

    /// Starts the profile's process when it never idles (`IDLE_SECS=0`), so a
    /// models refresh, including the first one at boot, lists its models.
    ///
    /// # Errors
    /// The `ensure_running` message when the process cannot start.
    pub async fn ensure_resident(&self, profile: &ProfileConfig) -> Result<(), String> {
        match &profile.managed {
            Some(command) if command.idle_timeout.is_none() => self.ensure_running(profile).await,
            _ => Ok(()),
        }
    }

    /// True when the profile's on-demand process is stopped on purpose: it has an
    /// idle timeout, no start failed recently, and nothing serves its port.
    pub async fn is_stopped(&self, profile: &ProfileConfig) -> bool {
        let Some(command) = profile
            .managed
            .as_ref()
            .filter(|c| c.idle_timeout.is_some())
        else {
            return false;
        };
        if self.is_running(&profile.name) {
            return false;
        }
        let failed = self
            .entries
            .get(&profile.name)
            .is_some_and(|entry| entry.backoff_reason().is_some());
        !failed && !accepts_connections(&command.address).await
    }

    /// Records a use so the idle timer restarts, e.g. when a long response arrives.
    pub fn touch(&self, profile: &str) {
        if let Some(entry) = self.entries.get(profile) {
            entry.touch();
        }
    }

    /// Whether llm-hub's own process for the profile is alive. A server that
    /// another process runs on the same port reports `false`, as does one still starting.
    pub fn is_running(&self, profile: &str) -> bool {
        let Some(entry) = self.entries.get(profile).map(|e| Arc::clone(&e)) else {
            return false;
        };
        let Ok(mut child) = entry.child.try_lock() else {
            return false;
        };
        child.as_mut().is_some_and(is_alive)
    }

    /// Stops processes whose profile was removed or whose command changed.
    pub fn reconcile(&self, config: &HubConfig) {
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| {
                config.profile(entry.key()).and_then(|p| p.managed.as_ref()) != Some(&entry.command)
            })
            .map(|entry| entry.key().clone())
            .collect();
        for profile in stale {
            if let Some((_, entry)) = self.entries.remove(&profile) {
                tokio::spawn(async move { entry.stop().await });
            }
        }
    }

    /// Stops every managed process and waits for each one to exit.
    pub async fn stop_all(&self) {
        let entries: Vec<Arc<ManagedEntry>> =
            self.entries.iter().map(|e| Arc::clone(e.value())).collect();
        futures_util::future::join_all(entries.iter().map(|entry| entry.stop())).await;
    }

    /// Starts a background task that stops processes idle longer than their `idle_timeout`.
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(manager.timings.idle_check_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                manager.stop_idle().await;
            }
        });
    }

    async fn stop_idle(&self) {
        let entries: Vec<Arc<ManagedEntry>> =
            self.entries.iter().map(|e| Arc::clone(e.value())).collect();
        for entry in entries {
            if !entry.is_idle() {
                continue;
            }
            // A start in progress holds the lock; that entry is not idle.
            let Ok(mut child) = entry.child.try_lock() else {
                continue;
            };
            if !entry.is_idle() {
                continue;
            }
            if let Some(running) = child.take() {
                tracing::info!(profile = %entry.profile, "stopping idle managed process");
                terminate_within(running, entry.stop_grace).await;
            }
        }
    }

    fn entry_for(&self, profile: &str, command: &ManagedCommand) -> Arc<ManagedEntry> {
        let mut slot = self.entries.entry(profile.to_string()).or_insert_with(|| {
            Arc::new(ManagedEntry::new(
                profile,
                command,
                self.timings.managed_stop_grace,
            ))
        });
        if slot.command != *command {
            let previous = std::mem::replace(
                slot.value_mut(),
                Arc::new(ManagedEntry::new(
                    profile,
                    command,
                    self.timings.managed_stop_grace,
                )),
            );
            tokio::spawn(async move { previous.stop().await });
        }
        Arc::clone(slot.value())
    }

    fn log_path(&self, profile: &str) -> PathBuf {
        let relative = self.logs_dir.join(format!("{profile}.log"));
        std::path::absolute(&relative).unwrap_or(relative)
    }
}

impl ManagedEntry {
    fn new(profile: &str, command: &ManagedCommand, stop_grace: Duration) -> ManagedEntry {
        ManagedEntry {
            profile: profile.to_string(),
            command: command.clone(),
            child: tokio::sync::Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
            backoff_until: Mutex::new(None),
            backoff_secs: Mutex::new(5),
            last_exit_reason: Mutex::new(None),
            stop_grace,
        }
    }

    fn touch(&self) {
        if let Ok(mut last_used) = self.last_used.lock() {
            *last_used = Instant::now();
        }
    }

    fn is_idle(&self) -> bool {
        let Some(idle_timeout) = self.command.idle_timeout else {
            return false;
        };
        self.last_used
            .lock()
            .is_ok_and(|last_used| last_used.elapsed() >= idle_timeout)
    }

    fn backoff_reason(&self) -> Option<String> {
        let now = Instant::now();
        let should_wait = self
            .backoff_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| until > now);
        if !should_wait {
            return None;
        }
        self.last_exit_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
    }

    fn record_exit_reason(&self, reason: String) {
        if let Ok(mut last_exit_reason) = self.last_exit_reason.lock() {
            *last_exit_reason = Some(reason);
        }
        if let Ok(mut backoff_secs) = self.backoff_secs.lock() {
            if let Ok(mut until) = self.backoff_until.lock() {
                *until = Some(Instant::now() + std::time::Duration::from_secs(*backoff_secs));
            }
            *backoff_secs = (*backoff_secs).saturating_mul(2).clamp(5, 60);
        }
    }

    fn reset_backoff(&self) {
        if let Ok(mut until) = self.backoff_until.lock() {
            *until = None;
        }
        if let Ok(mut backoff_secs) = self.backoff_secs.lock() {
            *backoff_secs = 5;
        }
        if let Ok(mut last_exit_reason) = self.last_exit_reason.lock() {
            *last_exit_reason = None;
        }
    }

    async fn stop(&self) {
        if let Some(running) = self.child.lock().await.take() {
            tracing::info!(profile = %self.profile, "stopping managed process");
            terminate_within(running, self.stop_grace).await;
        }
    }
}

async fn start(profile: &str, command: &ManagedCommand, log_path: &Path) -> Result<Child, String> {
    let child = spawn_logged(&command.argv, log_path, profile)?;
    tracing::info!(
        profile,
        pid = child.id(),
        "started managed process, waiting for {}",
        command.address
    );
    wait_ready(child, command.start_timeout, || {
        accepts_connections(&command.address)
    })
    .await
    .map_err(|error| match error {
        NotReady::Exited(status) => format!("the process exited with {status} before it listened"),
        NotReady::TimedOut => format!(
            "nothing listened on {} within {:?}",
            command.address, command.start_timeout
        ),
    })
}

/// Why a spawned process never became ready.
#[derive(Debug)]
pub(crate) enum NotReady {
    /// The process exited before the probe succeeded.
    Exited(ExitStatus),
    /// The probe kept failing until the timeout; the process was terminated.
    TimedOut,
}

/// Runs `command_line` with stdin closed and stdout and stderr appended to `log_path`. On unix the
/// child leads its own process group, so `terminate` also reaches the processes it starts.
///
/// # Errors
/// A message when `command_line` is empty, the log file cannot be opened, or the program cannot run.
pub(crate) fn spawn_logged(
    command_line: &[String],
    log_path: &Path,
    label: &str,
) -> Result<Child, String> {
    let (program, args) = command_line.split_first().ok_or("the command is empty")?;
    let log = open_log(log_path).map_err(|e| format!("cannot open log file: {e}"))?;
    let stderr = log
        .try_clone()
        .map_err(|e| format!("cannot open log file: {e}"))?;
    let mut spawn = Command::new(program);
    spawn
        .args(args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .kill_on_drop(true);
    #[cfg(unix)]
    spawn.process_group(0);
    #[cfg(windows)]
    spawn.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    let child = spawn
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    #[cfg(windows)]
    windows_job::bind_to_hub_lifetime(&child, label);
    #[cfg(not(windows))]
    let _ = label;
    Ok(child)
}

/// Polls `probe` until it returns true and hands the child back.
///
/// # Errors
/// `NotReady::Exited` when the child exits first. `NotReady::TimedOut` when `timeout` passes;
/// the child is terminated before this returns.
pub(crate) async fn wait_ready<F, Fut>(
    mut child: Child,
    timeout: Duration,
    mut probe: F,
) -> Result<Child, NotReady>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(NotReady::Exited(status));
        }
        if probe().await {
            return Ok(child);
        }
        if Instant::now() >= deadline {
            terminate(child).await;
            return Err(NotReady::TimedOut);
        }
        tokio::time::sleep(MANAGED_READY_POLL_INTERVAL).await;
    }
}

/// Opens `path` for appending, creating it and its parent directories.
///
/// # Errors
/// The I/O error when the directory or the file cannot be created or opened.
pub(crate) fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

async fn accepts_connections(address: &str) -> bool {
    matches!(
        tokio::time::timeout(
            MANAGED_READY_POLL_INTERVAL,
            tokio::net::TcpStream::connect(address)
        )
        .await,
        Ok(Ok(_))
    )
}

/// Asks the process to exit (SIGTERM to its process group, or Ctrl+Break on Windows), and
/// kills it when the grace period runs out or the request cannot be delivered.
pub(crate) async fn terminate(child: Child) {
    terminate_within(child, MANAGED_STOP_GRACE).await;
}

/// Sends SIGKILL to the process group that `pid` leads. A start calls it when its owner drops
/// the start before the process answers `/health`.
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    signal_group(Some(pid), libc::SIGKILL);
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pid: u32) {}

pub(crate) async fn terminate_within(mut child: Child, grace: Duration) {
    let pid = child.id();
    if request_exit(&child) && tokio::time::timeout(grace, child.wait()).await.is_ok() {
        // The leader is reaped, so `child.id()` is gone. `pid` still names the group that
        // may hold members which ignored SIGTERM.
        kill_group(pid);
        return;
    }
    kill_group(pid);
    let _ = child.kill().await;
}

#[cfg(unix)]
fn request_exit(child: &Child) -> bool {
    signal_group(child.id(), libc::SIGTERM)
}

#[cfg(unix)]
fn kill_group(pid: Option<u32>) {
    signal_group(pid, libc::SIGKILL);
}

#[cfg(not(unix))]
fn kill_group(_pid: Option<u32>) {}

/// Sends `signal` to the process group that `pid` leads. Returns false when `pid` is unknown
/// or the signal cannot be sent, which includes a group that no longer exists.
#[cfg(unix)]
fn signal_group(pid: Option<u32>, signal: libc::c_int) -> bool {
    let Some(pid) = pid.and_then(|id| libc::pid_t::try_from(id).ok()) else {
        return false;
    };
    // SAFETY: kill(2) takes plain integers and touches no memory. `pid` is our own child,
    // which leads its own process group, so `-pid` names that group alone.
    unsafe { libc::kill(-pid, signal) == 0 }
}

#[cfg(windows)]
fn request_exit(child: &Child) -> bool {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

    let Some(pid) = child.id() else {
        return false;
    };
    // SAFETY: plain integers in, no memory touched. The child was spawned with
    // CREATE_NEW_PROCESS_GROUP, so `pid` names its group alone, never the hub's.
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
}

#[cfg(not(any(unix, windows)))]
fn request_exit(_child: &Child) -> bool {
    false
}

/// Windows has no parent-death signal, so every child joins one job object
/// whose only handle the hub holds. When the hub exits, however it exits,
/// Windows closes the handle and the job kills the children.
#[cfg(windows)]
mod windows_job {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::OnceLock;

    use tokio::process::Child;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    static KILL_ON_CLOSE_JOB: OnceLock<Option<OwnedHandle>> = OnceLock::new();

    pub fn bind_to_hub_lifetime(child: &Child, profile: &str) {
        let Some(job) = KILL_ON_CLOSE_JOB.get_or_init(create_kill_on_close_job) else {
            return;
        };
        let Some(process) = child.raw_handle() else {
            return;
        };
        // SAFETY: both handles are open. The job handle lives in a static and
        // `child` has not been reaped, so its process handle is still valid.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            tracing::warn!(
                profile,
                "managed process may outlive the hub: cannot add it to the job object: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    fn create_kill_on_close_job() -> Option<OwnedHandle> {
        // SAFETY: null attributes and a null name are valid arguments; the call
        // returns a new handle or null.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            tracing::warn!(
                "cannot create a job object for managed processes: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        // SAFETY: `raw` is a new, open handle that nothing else owns.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).ok()?;
        // SAFETY: `limits` is the struct this information class expects, and
        // `size` is its exact size.
        let applied = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size,
            )
        };
        if applied == 0 {
            tracing::warn!(
                "cannot make the job object kill managed processes: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn spawn_logged_empty_argv_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let error = spawn_logged(&[], &dir.path().join("a.log"), "test").unwrap_err();

        assert_eq!(error, "the command is empty");
    }

    #[tokio::test]
    async fn spawn_logged_missing_program_names_it() {
        let dir = tempfile::tempdir().unwrap();

        let error = spawn_logged(
            &argv(&["llm-hub-no-such-program"]),
            &dir.path().join("a.log"),
            "test",
        )
        .unwrap_err();

        assert!(
            error.starts_with("cannot run llm-hub-no-such-program"),
            "{error}"
        );
    }

    #[test]
    fn spawn_logged_log_path_under_a_file_fails_to_open_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();

        let error = spawn_logged(&argv(&["true"]), &blocker.join("a.log"), "test").unwrap_err();

        assert!(error.starts_with("cannot open log file"), "{error}");
    }

    #[test]
    fn open_log_missing_directories_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("a.log");

        open_log(&path).unwrap();

        assert!(path.is_file());
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn group_exists(pgid: libc::pid_t) -> bool {
            // SAFETY: kill(2) with signal 0 only checks that the group exists; it touches no memory.
            unsafe { libc::kill(-pgid, 0) == 0 }
        }

        async fn wait_until_group_gone(pgid: libc::pid_t) -> bool {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if !group_exists(pgid) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }

        fn pgid_of(child: &Child) -> libc::pid_t {
            libc::pid_t::try_from(child.id().unwrap()).unwrap()
        }

        #[tokio::test]
        async fn wait_ready_http_health_answers_returns_the_running_child() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/health"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            let url = format!("{}/health", server.uri());
            let client = crate::services::http_client::client();
            let dir = tempfile::tempdir().unwrap();
            let child =
                spawn_logged(&argv(&["sleep", "30"]), &dir.path().join("a.log"), "test").unwrap();

            let mut ready = wait_ready(child, Duration::from_secs(5), || async {
                client
                    .get(&url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
            })
            .await
            .unwrap();

            assert!(is_alive(&mut ready));
            terminate(ready).await;
        }

        #[tokio::test]
        async fn wait_ready_probe_never_succeeds_times_out_and_stops_the_child() {
            let dir = tempfile::tempdir().unwrap();
            let child =
                spawn_logged(&argv(&["sleep", "30"]), &dir.path().join("a.log"), "test").unwrap();
            let pgid = pgid_of(&child);

            let result = wait_ready(child, Duration::ZERO, || async { false }).await;

            assert!(matches!(result, Err(NotReady::TimedOut)), "{result:?}");
            assert!(wait_until_group_gone(pgid).await);
        }

        #[tokio::test]
        async fn wait_ready_process_exits_first_reports_its_status() {
            let dir = tempfile::tempdir().unwrap();
            let child = spawn_logged(
                &argv(&["sh", "-c", "exit 3"]),
                &dir.path().join("a.log"),
                "test",
            )
            .unwrap();

            let result = wait_ready(child, Duration::from_secs(5), || async { false }).await;

            let Err(NotReady::Exited(status)) = result else {
                panic!("expected an early exit, got {result:?}");
            };
            assert_eq!(status.code(), Some(3));
        }

        #[tokio::test]
        async fn spawn_logged_existing_log_is_appended_with_stdout_and_stderr() {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("a.log");
            std::fs::write(&log, "earlier\n").unwrap();

            let mut child =
                spawn_logged(&argv(&["sh", "-c", "echo out; echo err >&2"]), &log, "test").unwrap();
            child.wait().await.unwrap();

            assert_eq!(
                std::fs::read_to_string(&log).unwrap(),
                "earlier\nout\nerr\n"
            );
        }

        #[tokio::test]
        async fn terminate_child_with_background_job_stops_the_whole_group() {
            let dir = tempfile::tempdir().unwrap();
            let child = spawn_logged(
                &argv(&["sh", "-c", "sleep 30 & sleep 30"]),
                &dir.path().join("a.log"),
                "test",
            )
            .unwrap();
            let pgid = pgid_of(&child);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(group_exists(pgid));

            terminate(child).await;

            assert!(wait_until_group_gone(pgid).await);
        }

        #[tokio::test]
        async fn terminate_within_group_ignores_sigterm_kills_it_after_the_grace() {
            let dir = tempfile::tempdir().unwrap();
            let child = spawn_logged(
                &argv(&["sh", "-c", "trap '' TERM; sleep 30 & sleep 30"]),
                &dir.path().join("a.log"),
                "test",
            )
            .unwrap();
            let pgid = pgid_of(&child);
            tokio::time::sleep(Duration::from_millis(200)).await;

            terminate_within(child, Duration::from_millis(200)).await;

            assert!(wait_until_group_gone(pgid).await);
        }

        #[tokio::test]
        async fn terminate_within_kills_a_group_member_that_ignores_sigterm_after_the_leader_exits()
        {
            let dir = tempfile::tempdir().unwrap();
            let child = spawn_logged(
                &argv(&[
                    "sh",
                    "-c",
                    "trap 'exit 0' TERM; (trap '' TERM; exec sleep 30) & wait",
                ]),
                &dir.path().join("a.log"),
                "test",
            )
            .unwrap();
            let pgid = pgid_of(&child);
            tokio::time::sleep(Duration::from_millis(200)).await;

            terminate_within(child, Duration::from_secs(5)).await;

            assert!(wait_until_group_gone(pgid).await);
        }
    }
}

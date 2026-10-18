use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;
use tokio::process::{Child, Command};

use crate::configs::{HubConfig, ManagedCommand, ProfileConfig};
use crate::consts::{MANAGED_IDLE_CHECK_INTERVAL, MANAGED_READY_POLL_INTERVAL, MANAGED_STOP_GRACE};

/// Starts each profile's local server on demand and stops it when idle.
pub struct ProcessManager {
    entries: DashMap<String, Arc<ManagedEntry>>,
    logs_dir: PathBuf,
}

/// One profile's process. The async mutex serialises start and stop, so
/// concurrent requests share a single start.
struct ManagedEntry {
    profile: String,
    command: ManagedCommand,
    child: tokio::sync::Mutex<Option<Child>>,
    last_used: Mutex<Instant>,
}

impl ProcessManager {
    /// Creates a manager that writes each process's output to `<logs_dir>/<profile>.log`.
    pub fn new(logs_dir: impl Into<PathBuf>) -> ProcessManager {
        ProcessManager {
            entries: DashMap::new(),
            logs_dir: logs_dir.into(),
        }
    }

    /// Makes sure the profile's server accepts connections. A profile without
    /// a command, or a port another process already serves, needs nothing.
    ///
    /// # Errors
    /// A message naming the profile and its log file when the process cannot
    /// be spawned, exits early, or does not listen within the start timeout.
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
        *child = None;
        if accepts_connections(&command.address).await {
            return Ok(());
        }
        let log_path = self.log_path(&profile.name);
        let started = start(&profile.name, command, &log_path)
            .await
            .map_err(|reason| {
                format!(
                    "managed process for profile {} did not start: {reason}; see {}",
                    profile.name,
                    log_path.display()
                )
            })?;
        *child = Some(started);
        entry.touch();
        Ok(())
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
            let mut interval = tokio::time::interval(MANAGED_IDLE_CHECK_INTERVAL);
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
                terminate(running).await;
            }
        }
    }

    fn entry_for(&self, profile: &str, command: &ManagedCommand) -> Arc<ManagedEntry> {
        let mut slot = self
            .entries
            .entry(profile.to_string())
            .or_insert_with(|| Arc::new(ManagedEntry::new(profile, command)));
        if slot.command != *command {
            let previous = std::mem::replace(
                slot.value_mut(),
                Arc::new(ManagedEntry::new(profile, command)),
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
    fn new(profile: &str, command: &ManagedCommand) -> ManagedEntry {
        ManagedEntry {
            profile: profile.to_string(),
            command: command.clone(),
            child: tokio::sync::Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
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

    async fn stop(&self) {
        if let Some(running) = self.child.lock().await.take() {
            tracing::info!(profile = %self.profile, "stopping managed process");
            terminate(running).await;
        }
    }
}

async fn start(profile: &str, command: &ManagedCommand, log_path: &Path) -> Result<Child, String> {
    let (program, args) = command.argv.split_first().ok_or("the command is empty")?;
    let log = open_log(log_path).map_err(|e| format!("cannot open log file: {e}"))?;
    let stderr = log
        .try_clone()
        .map_err(|e| format!("cannot open log file: {e}"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    tracing::info!(
        profile,
        pid = child.id(),
        "started managed process, waiting for {}",
        command.address
    );

    let deadline = Instant::now() + command.start_timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the process exited with {status} before it listened"
            ));
        }
        if accepts_connections(&command.address).await {
            return Ok(child);
        }
        if Instant::now() >= deadline {
            terminate(child).await;
            return Err(format!(
                "nothing listened on {} within {}s",
                command.address,
                command.start_timeout.as_secs()
            ));
        }
        tokio::time::sleep(MANAGED_READY_POLL_INTERVAL).await;
    }
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
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

/// SIGTERM, then SIGKILL once the grace period runs out.
async fn terminate(mut child: Child) {
    request_exit(&child);
    if tokio::time::timeout(MANAGED_STOP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

#[cfg(unix)]
fn request_exit(child: &Child) {
    let Some(pid) = child.id().and_then(|id| libc::pid_t::try_from(id).ok()) else {
        return;
    };
    // SAFETY: kill(2) takes plain integers and touches no memory. `pid` is our
    // own child, not yet reaped, so the id cannot belong to another process.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn request_exit(_child: &Child) {}

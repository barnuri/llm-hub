//! Pool of local model processes, one per registry model.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio::sync::Notify;

use super::command::build_argv;
use super::memory::{self, LoadedModel, NoRoom};
use super::registry_rows::{self, Engine, RegistryRow};
use crate::configs::{HubConfig, LocalServingConfig};
use crate::consts::MANAGED_IDLE_CHECK_INTERVAL;
use crate::services::managed_process::{NotReady, spawn_logged, terminate, wait_ready};

const LOCAL_LOGS_SUBDIR: &str = "local";
const PID_FILE: &str = "pids.json";
const REGISTRY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const LOOPBACK: &str = "127.0.0.1";
const HEALTH_PATH: &str = "/health";
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 60;
const NO_ROOM: &str = "no room to start local model";

/// Starts each `<profile>/<id>` registry model of a `KIND=local` profile on demand, and stops
/// it when idle, when memory is needed, or when its registry row changes.
pub struct LocalPool {
    profiles: ArcSwap<HashMap<String, Arc<LocalProfile>>>,
    entries: DashMap<String, Arc<ModelEntry>>,
    logs_dir: PathBuf,
    home: Option<String>,
    released: Arc<Notify>,
    budget_lock: Mutex<()>,
    pid_file_lock: Mutex<()>,
    health: reqwest::Client,
    budget_off_logged: Once,
}

/// Where a model process is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LocalModelState {
    /// No process runs.
    #[default]
    Stopped,
    /// The process runs but does not answer `/health` yet.
    Starting,
    /// The process answers `/health`.
    Running,
    /// The last start failed; new requests fail fast until the backoff passes.
    Failed,
}

/// One model of the pool, as the tuner and the admin page see it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalModelSnapshot {
    /// The local profile that serves the model.
    pub profile: String,
    /// The registry id of the model.
    pub id: String,
    /// The process state.
    pub state: LocalModelState,
    /// The port of the process, when one was picked.
    pub port: Option<u16>,
    /// The process id, when a process runs.
    pub pid: Option<u32>,
    /// The full command line of the process.
    pub argv: Vec<String>,
    /// The engine of the registry row that started the process.
    pub engine: Option<Engine>,
    /// When the process started.
    pub started: Option<SystemTime>,
    /// When the model last served a request.
    pub last_used: Instant,
    /// The number of requests the model is serving now.
    pub in_flight: usize,
    /// The memory estimate of the model in GB, 0 when no process holds memory.
    pub estimate_gb: f64,
}

/// Holds a model busy while a request uses it. Dropping it marks the request done.
pub struct Lease {
    base_url: String,
    entry: Arc<ModelEntry>,
    released: Arc<Notify>,
}

struct LocalProfile {
    config: LocalServingConfig,
    rows: ArcSwap<Vec<RegistryRow>>,
    stamp: Mutex<Option<RegistryStamp>>,
}

type RegistryStamp = (SystemTime, u64);

struct ModelEntry {
    key: String,
    profile: String,
    id: String,
    process: tokio::sync::Mutex<Option<Child>>,
    details: Mutex<Details>,
    last_used: Mutex<Instant>,
    in_flight: AtomicUsize,
    stop_when_idle: AtomicBool,
    backoff: Mutex<Backoff>,
}

#[derive(Clone, Default)]
struct Details {
    state: LocalModelState,
    row: Option<RegistryRow>,
    estimate_gb: f64,
    model_path: Option<PathBuf>,
    port: Option<u16>,
    pid: Option<u32>,
    argv: Vec<String>,
    started: Option<SystemTime>,
}

struct Backoff {
    until: Option<Instant>,
    secs: u64,
    reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PidRecord {
    key: String,
    pid: u32,
    port: u16,
    model_path: PathBuf,
}

impl LocalPool {
    /// Creates a pool for every `KIND=local` profile of `config`, logging each model process to
    /// `<logs_dir>/local/<id>.log`. It reads each registry once and starts no process.
    pub fn new(config: &HubConfig, logs_dir: impl AsRef<Path>) -> LocalPool {
        let relative = logs_dir.as_ref().join(LOCAL_LOGS_SUBDIR);
        let pool = LocalPool {
            profiles: ArcSwap::from_pointee(HashMap::new()),
            entries: DashMap::new(),
            logs_dir: std::path::absolute(&relative).unwrap_or(relative),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok(),
            released: Arc::new(Notify::new()),
            budget_lock: Mutex::new(()),
            pid_file_lock: Mutex::new(()),
            health: reqwest::Client::builder()
                .no_proxy()
                .timeout(HEALTH_PROBE_TIMEOUT)
                .build()
                .unwrap_or_default(),
            budget_off_logged: Once::new(),
        };
        pool.reconcile(config);
        pool
    }

    /// Stops the orphans a previous hub left in the pid file, then starts the idle reaper and
    /// the registry watcher. Call it once at startup, before the hub serves requests.
    pub fn spawn_background(self: &Arc<Self>) {
        self.kill_orphans();
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(MANAGED_IDLE_CHECK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                pool.stop_idle().await;
            }
        });
        let pool = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REGISTRY_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                pool.poll_registries();
            }
        });
    }

    /// Takes the local profiles of a new config. The pool stops the models of a profile that
    /// was removed or whose local settings changed.
    pub fn reconcile(&self, config: &HubConfig) {
        let current = self.profiles.load_full();
        let mut next = HashMap::new();
        for profile in &config.profiles {
            let Some(local) = &profile.local else {
                continue;
            };
            let kept = current
                .get(&profile.name)
                .filter(|existing| existing.config == *local)
                .map(Arc::clone);
            let entry = kept.unwrap_or_else(|| Arc::new(LocalProfile::load(local.clone())));
            next.insert(profile.name.clone(), entry);
        }
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|entry| {
                let kept = next.get(&entry.profile);
                let same = current.get(&entry.profile);
                !matches!((kept, same), (Some(a), Some(b)) if Arc::ptr_eq(a, b))
            })
            .map(|entry| entry.key().clone())
            .collect();
        self.profiles.store(Arc::new(next));
        for key in stale {
            if let Some((_, entry)) = self.entries.remove(&key) {
                let released = Arc::clone(&self.released);
                tokio::spawn(async move {
                    if let Some(child) = entry.take_process().await {
                        tracing::info!(model = %entry.key, "stopping local model of a changed profile");
                        terminate(child).await;
                    }
                    released.notify_waiters();
                });
            }
        }
    }

    /// Stops every model process and waits for each one to exit.
    pub async fn stop_all(&self) {
        let entries = self.all_entries();
        let children =
            futures_util::future::join_all(entries.iter().map(|entry| entry.take_process())).await;
        futures_util::future::join_all(children.into_iter().flatten().map(terminate)).await;
        self.write_pid_file();
        self.released.notify_waiters();
    }

    async fn stop_idle(&self) {
        let profiles = self.profiles.load_full();
        let mut stopping = Vec::new();
        let mut changed = false;
        for entry in self.all_entries() {
            if entry.in_flight() > 0 {
                continue;
            }
            // A start or an acquire in progress holds the lock; that model is busy.
            let Ok(mut process) = entry.process.try_lock() else {
                continue;
            };
            let Some(child) = process.as_mut() else {
                continue;
            };
            if !is_alive(child) {
                tracing::warn!(model = %entry.key, "local model process exited on its own");
                process.take();
                entry.set_stopped(LocalModelState::Stopped);
                changed = true;
                continue;
            }
            let ttl = profiles
                .get(&entry.profile)
                .map(|profile| profile.config.idle_timeout_for(&entry.id));
            let due = match ttl {
                None => true,
                Some(ttl) => {
                    entry.stop_when_idle.load(Ordering::SeqCst)
                        || ttl.is_some_and(|ttl| entry.idle_for() >= ttl)
                }
            };
            if !due || entry.in_flight() > 0 {
                continue;
            }
            if let Some(child) = process.take() {
                tracing::info!(model = %entry.key, "stopping idle local model");
                entry.set_stopped(LocalModelState::Stopped);
                stopping.push(child);
            }
        }
        changed |= !stopping.is_empty();
        futures_util::future::join_all(stopping.into_iter().map(terminate)).await;
        if changed {
            self.write_pid_file();
            self.released.notify_waiters();
        }
    }

    fn poll_registries(&self) {
        for profile in self.profiles.load().values() {
            let stamp = registry_stamp(&profile.config.registry);
            if stamp != *lock(&profile.stamp) {
                self.reload_profile(profile);
            }
        }
    }

    fn reload_profile(&self, profile: &LocalProfile) {
        let path = &profile.config.registry;
        *lock(&profile.stamp) = registry_stamp(path);
        let rows = match registry_rows::read(path) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(registry = %path.display(), "cannot read the local model registry: {e}");
                return;
            }
        };
        for entry in self.all_entries() {
            if !self.serves(&entry, profile) {
                continue;
            }
            let Some(old) = entry.details().row else {
                continue;
            };
            if rows.iter().find(|row| row.id == entry.id) != Some(&old) {
                tracing::info!(model = %entry.key, "registry row changed; restarting the model when idle");
                entry.stop_when_idle.store(true, Ordering::SeqCst);
            }
        }
        profile.rows.store(Arc::new(rows));
    }

    fn serves(&self, entry: &ModelEntry, profile: &LocalProfile) -> bool {
        self.profiles
            .load()
            .get(&entry.profile)
            .is_some_and(|candidate| std::ptr::eq(candidate.as_ref(), profile))
    }

    fn kill_orphans(&self) {
        let _guard = lock(&self.pid_file_lock);
        let path = self.logs_dir.join(PID_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let records: Vec<PidRecord> = serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(file = %path.display(), "cannot parse the local model pid file: {e}");
            Vec::new()
        });
        let own: HashSet<u32> = self
            .all_entries()
            .iter()
            .filter_map(|entry| entry.details().pid)
            .collect();
        for record in records.iter().filter(|r| !own.contains(&r.pid)) {
            if command_line(record.pid).is_some_and(|line| record.matches(&line)) {
                tracing::info!(model = %record.key, pid = record.pid, "stopping an orphaned local model process");
                signal_orphan(record.pid);
            }
        }
        self.write_pid_file_locked();
    }

    fn write_pid_file(&self) {
        let _guard = lock(&self.pid_file_lock);
        self.write_pid_file_locked();
    }

    fn write_pid_file_locked(&self) {
        let records: Vec<PidRecord> = self
            .all_entries()
            .iter()
            .filter_map(|entry| entry.pid_record())
            .collect();
        let path = self.logs_dir.join(PID_FILE);
        let written = serde_json::to_vec_pretty(&records)
            .map_err(std::io::Error::other)
            .and_then(|json| {
                std::fs::create_dir_all(&self.logs_dir)?;
                std::fs::write(&path, json)
            });
        if let Err(e) = written {
            tracing::warn!(file = %path.display(), "cannot write the local model pid file: {e}");
        }
    }

    fn all_entries(&self) -> Vec<Arc<ModelEntry>> {
        self.entries.iter().map(|e| Arc::clone(e.value())).collect()
    }

    #[allow(
        dead_code,
        reason = "only tests and the unused touch and restart methods look a model up"
    )]
    fn entry(&self, profile: &str, id: &str) -> Option<Arc<ModelEntry>> {
        self.entries
            .get(&model_key(profile, id))
            .map(|e| Arc::clone(e.value()))
    }
}

impl LocalPool {
    /// Whether an `acquire` error says the memory budget had no room before the start timeout,
    /// so a later request may succeed once a running model goes idle.
    pub fn is_no_room(error: &str) -> bool {
        error.starts_with(NO_ROOM)
    }

    /// The registry rows of local profile `profile` as the last registry read left them, or
    /// `None` when the profile is not local. It reads no file and starts no process.
    pub fn rows(&self, profile: &str) -> Option<Arc<Vec<RegistryRow>>> {
        self.profiles
            .load()
            .get(profile)
            .map(|local| local.rows.load_full())
    }

    /// Returns a lease on the running process of model `id` of `profile`, and starts the
    /// process first when none runs. Concurrent calls for one model share one start.
    ///
    /// # Errors
    ///
    /// A message when the profile is not local, the id is not in the registry, the model file
    /// is missing, the memory budget has no room within the start timeout, or the process does
    /// not answer `/health` in time. A failed start names the log file, and later calls fail
    /// fast with the same message until a backoff (5 s doubling to 60 s) passes.
    pub async fn acquire(&self, profile: &str, id: &str) -> Result<Lease, String> {
        let local = self
            .profiles
            .load()
            .get(profile)
            .map(Arc::clone)
            .ok_or_else(|| format!("profile {profile} does not serve local models"))?;
        let row = local.row(id).ok_or_else(|| {
            format!(
                "model {id} is not in the local model registry {}",
                local.config.registry.display()
            )
        })?;
        let entry = self.entry_for(profile, id);
        entry.touch();
        let mut process = entry.process.lock().await;
        if let Some(child) = process.as_mut()
            && is_alive(child)
        {
            let stale = entry.stop_when_idle.load(Ordering::SeqCst);
            if !stale || entry.in_flight() > 0 {
                return Ok(self.lease(&entry));
            }
            tracing::info!(model = %entry.key, "restarting the local model for its new registry row");
        }
        if let Some(child) = process.take() {
            entry.set_stopped(LocalModelState::Stopped);
            terminate(child).await;
            self.write_pid_file();
        }
        if let Some(reason) = lock(&entry.backoff).reason() {
            return Err(reason);
        }
        let model_path = registry_rows::resolve_model_file(
            &row,
            &local.config.models_dir,
            &local.config.hf_cache,
        )?;
        let estimate_gb = memory::estimate_gb(&row, &model_path);
        self.reserve_memory(&local, &entry, &row, estimate_gb)
            .await?;
        match self.launch(&local, &row, &entry, &model_path).await {
            Ok(child) => {
                entry.update(|d| d.state = LocalModelState::Running);
                *process = Some(child);
                lock(&entry.backoff).reset();
                self.write_pid_file();
                Ok(self.lease(&entry))
            }
            Err(reason) => {
                let message = format!(
                    "local model {} did not start: {reason}; see {}",
                    entry.key,
                    self.log_path(id).display()
                );
                lock(&entry.backoff).record(message.clone());
                entry.set_stopped(LocalModelState::Failed);
                self.released.notify_waiters();
                Err(message)
            }
        }
    }

    /// Records a use of a model, so its idle timer restarts.
    #[allow(
        dead_code,
        reason = "a lease touches its model on acquire and release, so no route calls this"
    )]
    pub fn touch(&self, profile: &str, id: &str) {
        if let Some(entry) = self.entry(profile, id) {
            entry.touch();
        }
    }

    /// Stops the model now, even with requests in flight, and clears its start backoff, so the
    /// next `acquire` starts a fresh process.
    #[allow(
        dead_code,
        reason = "no route or tuner restarts a local model; only tests call this"
    )]
    pub async fn restart(&self, profile: &str, id: &str) {
        let Some(entry) = self.entry(profile, id) else {
            return;
        };
        if let Some(child) = entry.take_process().await {
            tracing::info!(model = %entry.key, "restarting local model");
            terminate(child).await;
        }
        lock(&entry.backoff).reset();
        self.write_pid_file();
        self.released.notify_waiters();
    }

    /// Stops the model once no request is in flight, so the next `acquire` starts it fresh.
    ///
    /// # Errors
    ///
    /// A message when requests still use the model after `timeout`.
    #[allow(
        dead_code,
        reason = "no route or tuner restarts a local model; only tests call this"
    )]
    pub async fn restart_when_idle(
        &self,
        profile: &str,
        id: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let Some(entry) = self.entry(profile, id) else {
            return Ok(());
        };
        let deadline = Instant::now() + timeout;
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if entry.in_flight() == 0
                && let Ok(mut process) = entry.process.try_lock()
                && entry.in_flight() == 0
            {
                let child = process.take();
                entry.set_stopped(LocalModelState::Stopped);
                lock(&entry.backoff).reset();
                drop(process);
                if let Some(child) = child {
                    tracing::info!(model = %entry.key, "restarting idle local model");
                    terminate(child).await;
                }
                self.write_pid_file();
                self.released.notify_waiters();
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "local model {} still served requests after {}s; the hub did not restart it",
                    entry.key,
                    timeout.as_secs()
                ));
            }
            let _ =
                tokio::time::timeout(remaining.min(MANAGED_IDLE_CHECK_INTERVAL), released).await;
        }
    }

    /// Reloads every local registry now, as the 2 s watcher does when a file changes.
    #[allow(
        dead_code,
        reason = "the registry watcher reloads through `poll_registries`; only tests call this"
    )]
    pub fn reload_now(&self) {
        for profile in self.profiles.load().values() {
            self.reload_profile(profile);
        }
    }

    /// The state of every model the pool has seen, sorted by profile and id.
    pub fn snapshot(&self) -> Vec<LocalModelSnapshot> {
        let mut models: Vec<LocalModelSnapshot> = self
            .all_entries()
            .iter()
            .map(|entry| entry.snapshot())
            .collect();
        models.sort_by(|a, b| (&a.profile, &a.id).cmp(&(&b.profile, &b.id)));
        models
    }

    async fn reserve_memory(
        &self,
        local: &LocalProfile,
        entry: &ModelEntry,
        row: &RegistryRow,
        estimate_gb: f64,
    ) -> Result<(), String> {
        let Some(budget_gb) = memory::budget_gb(local.config.memory_gb, memory::total_ram_bytes())
        else {
            self.budget_off_logged.call_once(|| {
                tracing::warn!("the hub cannot read the RAM size; the local memory budget is off");
            });
            entry.reserve(row, estimate_gb);
            return Ok(());
        };
        let timeout = local.config.start_timeout;
        let deadline = Instant::now() + timeout;
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            let (evicted, outcome) = self.try_reserve(entry, row, estimate_gb, budget_gb);
            if !evicted.is_empty() {
                futures_util::future::join_all(evicted.into_iter().map(terminate)).await;
                self.write_pid_file();
            }
            match outcome {
                Ok(()) => return Ok(()),
                Err(NoRoom::TooBig) => {
                    return Err(format!(
                        "local model {} needs about {estimate_gb:.1} GB, more than the whole local memory budget of {budget_gb:.1} GB",
                        entry.key
                    ));
                }
                Err(NoRoom::Wait) => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "{NO_ROOM} {}: it needs about {estimate_gb:.1} GB of the {budget_gb:.1} GB budget, and the models that use it still served requests after {}s",
                    entry.key,
                    timeout.as_secs()
                ));
            }
            let _ =
                tokio::time::timeout(remaining.min(MANAGED_IDLE_CHECK_INTERVAL), released).await;
        }
    }

    /// Stops the least recently used idle models until `estimate_gb` fits, and reserves it.
    /// Returns the stopped children, which the caller terminates.
    fn try_reserve(
        &self,
        entry: &ModelEntry,
        row: &RegistryRow,
        estimate_gb: f64,
        budget_gb: f64,
    ) -> (Vec<Child>, Result<(), NoRoom>) {
        let _guard = lock(&self.budget_lock);
        let others: Vec<Arc<ModelEntry>> = self
            .all_entries()
            .into_iter()
            .filter(|other| other.key != entry.key && other.holds_memory())
            .collect();
        let mut busy = HashSet::new();
        let mut evicted = Vec::new();
        loop {
            let loaded: Vec<LoadedModel> = others
                .iter()
                .filter(|other| other.holds_memory())
                .map(|other| other.loaded_model(busy.contains(&other.key)))
                .collect();
            let chosen = match memory::choose_evictions(&loaded, estimate_gb, budget_gb) {
                Ok(chosen) => chosen,
                Err(no_room) => return (evicted, Err(no_room)),
            };
            if chosen.is_empty() {
                entry.reserve(row, estimate_gb);
                return (evicted, Ok(()));
            }
            for key in chosen {
                let Some(other) = others.iter().find(|other| other.key == key) else {
                    continue;
                };
                match other.process.try_lock() {
                    Ok(mut process) if other.in_flight() == 0 => {
                        tracing::info!(model = %other.key, "stopping idle local model to free memory");
                        evicted.extend(process.take());
                        other.set_stopped(LocalModelState::Stopped);
                    }
                    _ => {
                        busy.insert(other.key.clone());
                    }
                }
            }
        }
    }

    async fn launch(
        &self,
        local: &LocalProfile,
        row: &RegistryRow,
        entry: &ModelEntry,
        model_path: &Path,
    ) -> Result<Child, String> {
        let port = free_port().map_err(|e| format!("cannot pick a free port: {e}"))?;
        let argv = build_argv(row, &local.config, model_path, port, self.home.as_deref())?;
        let child = spawn_logged(&argv, &self.log_path(&entry.id), &entry.key)?;
        let pid = child.id();
        entry.update(|d| {
            d.model_path = Some(model_path.to_path_buf());
            d.port = Some(port);
            d.pid = pid;
            d.argv = argv;
            d.started = Some(SystemTime::now());
        });
        tracing::info!(model = %entry.key, pid, port, "started local model, waiting for /health");
        let url = format!("http://{LOOPBACK}:{port}{HEALTH_PATH}");
        let timeout = local.config.start_timeout;
        wait_ready(child, timeout, || health_ok(&self.health, &url))
            .await
            .map_err(|error| match error {
                NotReady::Exited(status) => {
                    format!("the process exited with {status} before /health answered")
                }
                NotReady::TimedOut => {
                    format!("{url} did not answer 200 within {}s", timeout.as_secs())
                }
            })
    }

    fn lease(&self, entry: &Arc<ModelEntry>) -> Lease {
        entry.in_flight.fetch_add(1, Ordering::SeqCst);
        entry.touch();
        let port = entry.details().port.unwrap_or_default();
        Lease {
            base_url: format!("http://{LOOPBACK}:{port}"),
            entry: Arc::clone(entry),
            released: Arc::clone(&self.released),
        }
    }

    fn entry_for(&self, profile: &str, id: &str) -> Arc<ModelEntry> {
        let key = model_key(profile, id);
        let slot = self
            .entries
            .entry(key.clone())
            .or_insert_with(|| Arc::new(ModelEntry::new(key, profile, id)));
        Arc::clone(slot.value())
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.logs_dir.join(format!("{id}.log"))
    }
}

impl Lease {
    /// The base URL of the model process, `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.entry.touch();
        self.entry.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.released.notify_waiters();
    }
}

impl LocalProfile {
    fn load(config: LocalServingConfig) -> LocalProfile {
        let stamp = registry_stamp(&config.registry);
        let rows = registry_rows::read(&config.registry).unwrap_or_else(|e| {
            tracing::warn!(registry = %config.registry.display(), "cannot read the local model registry: {e}");
            Vec::new()
        });
        LocalProfile {
            config,
            rows: ArcSwap::from_pointee(rows),
            stamp: Mutex::new(stamp),
        }
    }

    fn row(&self, id: &str) -> Option<RegistryRow> {
        self.rows.load().iter().find(|row| row.id == id).cloned()
    }
}

impl ModelEntry {
    fn new(key: String, profile: &str, id: &str) -> ModelEntry {
        ModelEntry {
            key,
            profile: profile.to_owned(),
            id: id.to_owned(),
            process: tokio::sync::Mutex::new(None),
            details: Mutex::new(Details::default()),
            last_used: Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
            stop_when_idle: AtomicBool::new(false),
            backoff: Mutex::new(Backoff::default()),
        }
    }

    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        lock(&self.last_used).elapsed()
    }

    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    fn details(&self) -> Details {
        lock(&self.details).clone()
    }

    fn update(&self, change: impl FnOnce(&mut Details)) {
        change(&mut lock(&self.details));
    }

    fn reserve(&self, row: &RegistryRow, estimate_gb: f64) {
        self.stop_when_idle.store(false, Ordering::SeqCst);
        *lock(&self.details) = Details {
            state: LocalModelState::Starting,
            row: Some(row.clone()),
            estimate_gb,
            ..Details::default()
        };
    }

    fn set_stopped(&self, state: LocalModelState) {
        self.stop_when_idle.store(false, Ordering::SeqCst);
        *lock(&self.details) = Details {
            state,
            ..Details::default()
        };
    }

    async fn take_process(&self) -> Option<Child> {
        let child = self.process.lock().await.take();
        self.set_stopped(LocalModelState::Stopped);
        child
    }

    fn holds_memory(&self) -> bool {
        matches!(
            lock(&self.details).state,
            LocalModelState::Starting | LocalModelState::Running
        )
    }

    fn loaded_model(&self, busy: bool) -> LoadedModel {
        LoadedModel {
            id: self.key.clone(),
            estimate_gb: lock(&self.details).estimate_gb,
            last_used: *lock(&self.last_used),
            in_flight: if busy { 1 } else { self.in_flight() },
        }
    }

    fn pid_record(&self) -> Option<PidRecord> {
        let details = lock(&self.details);
        Some(PidRecord {
            key: self.key.clone(),
            pid: details.pid?,
            port: details.port?,
            model_path: details.model_path.clone()?,
        })
    }

    fn snapshot(&self) -> LocalModelSnapshot {
        let details = self.details();
        LocalModelSnapshot {
            profile: self.profile.clone(),
            id: self.id.clone(),
            state: details.state,
            port: details.port,
            pid: details.pid,
            argv: details.argv,
            engine: details.row.map(|row| row.engine),
            started: details.started,
            last_used: *lock(&self.last_used),
            in_flight: self.in_flight(),
            estimate_gb: details.estimate_gb,
        }
    }
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff {
            until: None,
            secs: BACKOFF_MIN_SECS,
            reason: None,
        }
    }
}

impl Backoff {
    fn reason(&self) -> Option<String> {
        self.until
            .is_some_and(|until| until > Instant::now())
            .then(|| self.reason.clone())
            .flatten()
    }

    fn record(&mut self, reason: String) {
        self.reason = Some(reason);
        self.until = Some(Instant::now() + Duration::from_secs(self.secs));
        self.secs = self
            .secs
            .saturating_mul(2)
            .clamp(BACKOFF_MIN_SECS, BACKOFF_MAX_SECS);
    }

    fn reset(&mut self) {
        *self = Backoff::default();
    }
}

impl PidRecord {
    /// Whether `command_line` is still this model's server: it holds the port and model path.
    fn matches(&self, command_line: &str) -> bool {
        command_line.contains(&format!("--port {}", self.port))
            && command_line.contains(&*self.model_path.to_string_lossy())
    }
}

fn model_key(profile: &str, id: &str) -> String {
    format!("{profile}/{id}")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

fn registry_stamp(path: &Path) -> Option<RegistryStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind((LOOPBACK, 0))?
        .local_addr()?
        .port())
}

async fn health_ok(client: &reqwest::Client, url: &str) -> bool {
    client
        .get(url)
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

#[cfg(unix)]
fn command_line(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(not(unix))]
fn command_line(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
fn signal_orphan(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: kill(2) takes plain integers and touches no memory. The hub started `pid` as the
    // leader of its own process group, and its command line still names the model's port and file.
    let group_signalled = unsafe { libc::kill(-pid, libc::SIGTERM) == 0 };
    if !group_signalled {
        // SAFETY: as above, for the process alone when its group is gone.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(not(unix))]
fn signal_orphan(_pid: u32) {}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::sync::OnceLock;

    use super::*;

    const PROFILE: &str = "mac";
    const START_LINE: &str = "fake server start";
    const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import http.server, sys
args = sys.argv[1:]
port = int(args[args.index("--port") + 1])
model = args[args.index("-m") + 1]
print("fake server start", flush=True)
if "broken" in model:
    print("cannot load the model", flush=True)
    sys.exit(3)
class Health(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ready = self.path == "/health" and "unready" not in model
        self.send_response(200 if ready else 503)
        self.end_headers()
    def log_message(self, *args):
        pass
http.server.HTTPServer(("127.0.0.1", port), Health).serve_forever()
"#;

    /// One fake server script per test binary, so no test writes an executable while another forks.
    fn fake_server() -> &'static Path {
        static SCRIPT: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
        let (_, path) = SCRIPT.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fake-llama");
            std::fs::write(&path, FAKE_SERVER).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        });
        path
    }

    fn no_local_config() -> HubConfig {
        HubConfig::from_map(&HashMap::from([
            ("LLM_HUB_PROFILES".into(), "openai".into()),
            (
                "LLM_HUB_OPENAI_BASE_URL".into(),
                "https://api.openai.com/v1".into(),
            ),
            ("LLM_HUB_OPENAI_API_KEY".into(), "sk-test".into()),
        ]))
        .unwrap()
    }

    struct Fixture {
        dir: tempfile::TempDir,
        pool: LocalPool,
    }

    impl Fixture {
        fn new(rows: &[(&str, f64)], extra: &[(&str, &str)]) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("models")).unwrap();
            let fixture = Fixture {
                pool: LocalPool::new(&no_local_config(), dir.path().join("logs")),
                dir,
            };
            fixture.write_registry(rows, 4096);
            fixture.pool.reconcile(&fixture.hub_config(extra));
            fixture
        }

        fn hub_config(&self, extra: &[(&str, &str)]) -> HubConfig {
            let path = |name: &str| self.dir.path().join(name).display().to_string();
            let mut vars: HashMap<String, String> = HashMap::from([
                ("LLM_HUB_PROFILES".into(), PROFILE.into()),
                ("LLM_HUB_MAC_KIND".into(), "local".into()),
                ("LLM_HUB_MAC_REGISTRY".into(), path("registry.txt")),
                ("LLM_HUB_MAC_MODELS_DIR".into(), path("models")),
                ("LLM_HUB_MAC_HF_CACHE".into(), path("hf")),
                (
                    "LLM_HUB_MAC_LLAMA_BIN".into(),
                    fake_server().display().to_string(),
                ),
                ("LLM_HUB_MAC_START_TIMEOUT_SECS".into(), "20".into()),
            ]);
            for (key, value) in extra {
                vars.insert(format!("LLM_HUB_MAC_{key}"), (*value).to_owned());
            }
            HubConfig::from_map(&vars).unwrap()
        }

        fn write_registry(&self, rows: &[(&str, f64)], ctx: u64) {
            let mut text = String::from("# id|repo|file|size_gb|ctx\n");
            for (id, size_gb) in rows {
                writeln!(text, "{id}|org/{id}|{id}.gguf|{size_gb}|{ctx}").unwrap();
                std::fs::write(self.dir.path().join(format!("models/{id}.gguf")), "x").unwrap();
            }
            std::fs::write(self.dir.path().join("registry.txt"), text).unwrap();
        }

        async fn acquire(&self, id: &str) -> Result<Lease, String> {
            self.pool.acquire(PROFILE, id).await
        }

        fn model(&self, id: &str) -> LocalModelSnapshot {
            self.pool
                .snapshot()
                .into_iter()
                .find(|model| model.id == id)
                .unwrap()
        }

        fn starts(&self, id: &str) -> usize {
            std::fs::read_to_string(self.pool.log_path(id))
                .unwrap_or_default()
                .matches(START_LINE)
                .count()
        }

        fn pid_file(&self) -> Vec<PidRecord> {
            let text = std::fs::read_to_string(self.pool.logs_dir.join(PID_FILE)).unwrap();
            serde_json::from_str(&text).unwrap()
        }

        fn age(&self, id: &str) {
            let entry = self.pool.entry(PROFILE, id).unwrap();
            *lock(&entry.last_used) = Instant::now().checked_sub(Duration::from_hours(1)).unwrap();
        }
    }

    fn is_running(pid: u32) -> bool {
        command_line(pid).is_some_and(|line| !line.is_empty())
    }

    async fn wait_until_gone(pid: u32) -> bool {
        for _ in 0..50 {
            if !is_running(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[tokio::test]
    async fn concurrent_acquires_share_one_start_and_leases_count_in_flight() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);

        let (first, second) = tokio::join!(fx.acquire("a"), fx.acquire("a"));
        let (first, second) = (first.unwrap(), second.unwrap());

        assert_eq!(fx.starts("a"), 1);
        assert_eq!(first.base_url(), second.base_url());
        let model = fx.model("a");
        assert_eq!(model.state, LocalModelState::Running);
        assert_eq!(model.in_flight, 2);
        assert_eq!(model.engine, Some(Engine::Llama));
        assert_eq!(
            first.base_url(),
            format!("http://127.0.0.1:{}", model.port.unwrap())
        );
        assert!(model.argv.contains(&"--port".to_owned()));
        assert!(model.estimate_gb > 0.0);
        let records = fx.pid_file();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, model.pid.unwrap());
        assert_eq!(records[0].key, "mac/a");

        drop(first);
        drop(second);
        assert_eq!(fx.model("a").in_flight, 0);
        let third = fx.acquire("a").await.unwrap();
        assert_eq!(fx.starts("a"), 1, "a running model is reused");
        drop(third);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn restart_starts_a_new_process_next_time() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        let old_pid = fx.model("a").pid.unwrap();

        fx.pool.restart(PROFILE, "a").await;
        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        assert!(wait_until_gone(old_pid).await);
        assert!(fx.pid_file().is_empty());

        drop(fx.acquire("a").await.unwrap());
        assert_eq!(fx.starts("a"), 2);
        assert_ne!(fx.model("a").pid, Some(old_pid));
        fx.pool.restart(PROFILE, "missing").await;
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn a_failed_start_names_the_log_and_backs_off() {
        let fx = Fixture::new(&[("broken", 1.0)], &[]);

        let err = fx.acquire("broken").await.err().unwrap();
        assert!(err.contains("exited"), "{err}");
        assert!(
            err.contains(&fx.pool.log_path("broken").display().to_string()),
            "{err}"
        );
        assert_eq!(fx.model("broken").state, LocalModelState::Failed);

        let again = fx.acquire("broken").await.err().unwrap();
        assert_eq!(again, err);
        assert_eq!(fx.starts("broken"), 1, "the backoff skips the spawn");

        fx.pool.restart(PROFILE, "broken").await;
        assert!(fx.acquire("broken").await.is_err());
        assert_eq!(fx.starts("broken"), 2, "restart clears the backoff");
    }

    #[tokio::test]
    async fn a_model_that_never_gets_healthy_times_out() {
        let fx = Fixture::new(&[("unready", 1.0)], &[("START_TIMEOUT_SECS", "1")]);

        let err = fx.acquire("unready").await.err().unwrap();

        assert!(
            err.contains("/health did not answer 200 within 1s"),
            "{err}"
        );
        assert_eq!(fx.model("unready").state, LocalModelState::Failed);
    }

    #[tokio::test]
    async fn unknown_profile_unknown_id_and_missing_file_fail_without_a_start() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        std::fs::remove_file(fx.dir.path().join("models/a.gguf")).unwrap();

        let profile = fx.pool.acquire("other", "a").await.err().unwrap();
        assert!(profile.contains("does not serve local models"), "{profile}");
        let id = fx.acquire("nope").await.err().unwrap();
        assert!(id.contains("not in the local model registry"), "{id}");
        let file = fx.acquire("a").await.err().unwrap();
        assert!(file.contains("not on disk"), "{file}");
        assert_eq!(fx.starts("a"), 0);
    }

    #[tokio::test]
    async fn the_reaper_stops_an_idle_model_past_its_ttl_but_never_one_in_flight() {
        let fx = Fixture::new(
            &[("a", 1.0), ("pinned", 1.0)],
            &[("MODEL_TTL_SECS", "pinned=0")],
        );
        let lease = fx.acquire("a").await.unwrap();
        drop(fx.acquire("pinned").await.unwrap());
        fx.age("a");
        fx.age("pinned");

        fx.pool.stop_idle().await;
        assert_eq!(fx.model("a").state, LocalModelState::Running);

        drop(lease);
        fx.age("a");
        fx.pool.stop_idle().await;
        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        assert_eq!(fx.model("pinned").state, LocalModelState::Running);
        assert_eq!(fx.pid_file().len(), 1);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn the_reaper_clears_a_process_that_exited_on_its_own() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        let pid = fx.model("a").pid.unwrap();
        // SAFETY: kill(2) on the test's own child process.
        unsafe {
            libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGKILL);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        fx.pool.stop_idle().await;

        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        assert!(fx.pid_file().is_empty());
    }

    #[tokio::test]
    async fn a_start_stops_the_least_recently_used_idle_model_to_fit() {
        let fx = Fixture::new(
            &[("a", 4.0), ("b", 4.0), ("c", 4.0)],
            &[("MEMORY_GB", "10")],
        );
        drop(fx.acquire("a").await.unwrap());
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(fx.acquire("b").await.unwrap());

        drop(fx.acquire("c").await.unwrap());

        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        assert_eq!(fx.model("b").state, LocalModelState::Running);
        assert_eq!(fx.model("c").state, LocalModelState::Running);
        assert_eq!(fx.pid_file().len(), 2);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn a_start_waits_for_a_lease_release_to_make_room() {
        let fx = Fixture::new(
            &[("a", 4.0), ("b", 4.0), ("c", 4.0)],
            &[("MEMORY_GB", "10")],
        );
        let a = fx.acquire("a").await.unwrap();
        let b = fx.acquire("b").await.unwrap();

        let (c, ()) = tokio::join!(fx.acquire("c"), async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(a);
        });

        assert!(c.is_ok());
        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        drop(b);
        drop(c);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn a_start_fails_when_no_lease_is_released_within_the_start_timeout() {
        let fx = Fixture::new(
            &[("a", 4.0), ("b", 4.0), ("c", 4.0)],
            &[("MEMORY_GB", "10"), ("START_TIMEOUT_SECS", "2")],
        );
        let a = fx.acquire("a").await.unwrap();
        let b = fx.acquire("b").await.unwrap();

        let err = fx.acquire("c").await.err().unwrap();

        assert!(err.contains("no room to start local model mac/c"), "{err}");
        assert_eq!(fx.starts("c"), 0);
        assert_eq!(fx.model("c").state, LocalModelState::Stopped);
        drop((a, b));
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn a_model_bigger_than_the_budget_fails_at_once() {
        let fx = Fixture::new(&[("big", 20.0)], &[("MEMORY_GB", "10")]);

        let err = fx.acquire("big").await.err().unwrap();

        assert!(
            err.contains("more than the whole local memory budget"),
            "{err}"
        );
        assert_eq!(fx.starts("big"), 0);
    }

    #[tokio::test]
    async fn a_changed_row_restarts_its_model_when_idle() {
        let fx = Fixture::new(&[("a", 1.0), ("b", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        let lease_b = fx.acquire("b").await.unwrap();
        let old_b = fx.model("b").pid;

        fx.write_registry(&[("a", 1.0), ("b", 1.0)], 16384);
        fx.pool.poll_registries();
        fx.pool.stop_idle().await;

        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        assert_eq!(fx.model("b").state, LocalModelState::Running);
        let still_old = fx.acquire("b").await.unwrap();
        assert_eq!(fx.model("b").pid, old_b, "a busy model keeps serving");
        drop((lease_b, still_old));

        drop(fx.acquire("b").await.unwrap());
        let b = fx.model("b");
        assert_ne!(b.pid, old_b);
        assert!(b.argv.contains(&"16384".to_owned()), "{:?}", b.argv);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn a_removed_row_stops_its_model_and_rejects_new_requests() {
        let fx = Fixture::new(&[("a", 1.0), ("b", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());

        fx.write_registry(&[("b", 1.0)], 4096);
        fx.pool.reload_now();
        fx.pool.poll_registries();
        fx.pool.stop_idle().await;

        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        let err = fx.acquire("a").await.err().unwrap();
        assert!(err.contains("not in the local model registry"), "{err}");
    }

    #[tokio::test]
    async fn an_unreadable_registry_keeps_the_last_rows() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        std::fs::remove_file(fx.dir.path().join("registry.txt")).unwrap();

        fx.pool.poll_registries();

        drop(fx.acquire("a").await.unwrap());
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn restart_when_idle_waits_for_the_last_lease() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        let lease = fx.acquire("a").await.unwrap();

        let busy = fx
            .pool
            .restart_when_idle(PROFILE, "a", Duration::from_millis(300))
            .await;
        assert!(busy.unwrap_err().contains("still served requests"));
        assert_eq!(fx.model("a").state, LocalModelState::Running);

        drop(lease);
        fx.pool
            .restart_when_idle(PROFILE, "a", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(fx.model("a").state, LocalModelState::Stopped);
        fx.pool
            .restart_when_idle(PROFILE, "missing", Duration::ZERO)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn touch_restarts_the_idle_timer() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        fx.age("a");

        fx.pool.touch(PROFILE, "a");
        fx.pool.touch(PROFILE, "missing");
        fx.pool.stop_idle().await;

        assert_eq!(fx.model("a").state, LocalModelState::Running);
        fx.pool.stop_all().await;
    }

    #[tokio::test]
    async fn stop_all_stops_every_child_and_empties_the_pid_file() {
        let fx = Fixture::new(&[("a", 1.0), ("b", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        drop(fx.acquire("b").await.unwrap());
        let pids = [fx.model("a").pid.unwrap(), fx.model("b").pid.unwrap()];

        fx.pool.stop_all().await;

        for pid in pids {
            assert!(wait_until_gone(pid).await, "pid {pid} still runs");
        }
        assert!(
            fx.pool
                .snapshot()
                .iter()
                .all(|model| model.state == LocalModelState::Stopped)
        );
        assert!(fx.pid_file().is_empty());
    }

    #[tokio::test]
    async fn a_config_without_the_profile_stops_its_models() {
        let fx = Fixture::new(&[("a", 1.0)], &[]);
        drop(fx.acquire("a").await.unwrap());
        let pid = fx.model("a").pid.unwrap();

        fx.pool.reconcile(&fx.hub_config(&[]));
        assert_eq!(
            fx.model("a").pid,
            Some(pid),
            "an unchanged profile keeps its models"
        );

        fx.pool.reconcile(&no_local_config());

        assert!(fx.pool.snapshot().is_empty());
        assert!(wait_until_gone(pid).await);
        let err = fx.acquire("a").await.err().unwrap();
        assert!(err.contains("does not serve local models"), "{err}");
    }

    #[tokio::test]
    async fn the_default_config_has_no_local_models_and_background_tasks_idle() {
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(LocalPool::new(&no_local_config(), dir.path()));

        pool.spawn_background();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(pool.snapshot().is_empty());
        assert!(!dir.path().join("local").exists(), "no pid file is written");
        pool.reload_now();
        pool.stop_all().await;
    }

    #[tokio::test]
    async fn boot_kills_only_orphans_that_still_run_their_model_command() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("orphan.gguf");
        let spawn = |args: &[&str]| {
            let mut command = std::process::Command::new("sh");
            command.args(args).process_group(0);
            command.spawn().unwrap()
        };
        let model_arg = model.display().to_string();
        let mut orphan = spawn(&["-c", "sleep 300; :", "sh", "--port", "4242", &model_arg]);
        let mut stranger = spawn(&["-c", "sleep 300; :", "sh", "--port", "4243"]);
        let records = vec![
            PidRecord {
                key: "mac/orphan".into(),
                pid: orphan.id(),
                port: 4242,
                model_path: model.clone(),
            },
            PidRecord {
                key: "mac/stranger".into(),
                pid: stranger.id(),
                port: 4243,
                model_path: model,
            },
        ];
        let local = dir.path().join("local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join(PID_FILE), serde_json::to_vec(&records).unwrap()).unwrap();
        let pool = LocalPool::new(&no_local_config(), dir.path());

        pool.kill_orphans();

        let mut exited = None;
        for _ in 0..50 {
            exited = orphan.try_wait().unwrap();
            if exited.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(exited.is_some(), "the orphan still runs");
        assert!(
            stranger.try_wait().unwrap().is_none(),
            "a non-matching process survives"
        );
        let rewritten = std::fs::read_to_string(local.join(PID_FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<PidRecord>>(&rewritten)
                .unwrap()
                .len(),
            0
        );
        stranger.kill().unwrap();
        stranger.wait().unwrap();
    }

    #[test]
    fn a_corrupt_pid_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join(PID_FILE), "not json").unwrap();
        let pool = LocalPool::new(&no_local_config(), dir.path());

        pool.kill_orphans();

        assert_eq!(std::fs::read_to_string(local.join(PID_FILE)).unwrap(), "[]");
    }

    #[test]
    fn a_pid_record_matches_only_its_port_and_model_path() {
        let record = PidRecord {
            key: "mac/a".into(),
            pid: 1,
            port: 8123,
            model_path: PathBuf::from("/m/a.gguf"),
        };
        assert!(record.matches("llama serve -m /m/a.gguf --port 8123 --ctx-size 4096"));
        assert!(!record.matches("llama serve -m /m/a.gguf --port 8124"));
        assert!(!record.matches("llama serve -m /m/b.gguf --port 8123"));
    }

    #[test]
    fn backoff_doubles_to_a_cap_and_resets() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.reason(), None);
        backoff.record("first".into());
        assert_eq!(backoff.reason().as_deref(), Some("first"));
        for _ in 0..6 {
            backoff.record("again".into());
        }
        assert_eq!(backoff.secs, BACKOFF_MAX_SECS);
        backoff.until = Instant::now().checked_sub(Duration::from_secs(1));
        assert_eq!(
            backoff.reason(),
            None,
            "a passed backoff lets a start through"
        );
        backoff.reset();
        assert_eq!(backoff.secs, BACKOFF_MIN_SECS);
        assert_eq!(backoff.reason, None);
    }
}

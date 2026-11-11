//! The Smart Auto and Copilot Auto selection history: a bounded in-memory record, the current
//! selection of each worker, and a writer thread that saves events off the request path.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use sha2::{Digest, Sha256};

use crate::schemas::current_selection::CurrentSelection;
use crate::schemas::history_query::HistoryQuery;
use crate::schemas::scope_kind::ScopeKind;
use crate::schemas::selection_event::SelectionEvent;
use crate::schemas::selection_stage::SelectionStage;
use crate::schemas::selection_state::SelectionState;
use crate::services::store::{SELECTION_RETENTION_EVENTS, Store, now_ms};
use crate::utils::hex::to_hex;

/// Events kept in memory for the history route. The newest are reloaded from the store on start.
const RING_CAPACITY: usize = 2_000;
/// Workers whose current selection is kept. The least recently updated worker goes first.
const CURRENT_CAPACITY: usize = 256;
/// A worker whose latest event is older than this is reported as stale.
const CURRENT_STALE_AFTER_MS: u64 = 10 * 60 * 1000;
/// Events queued for the writer. A full queue drops the event rather than block a request.
const WRITE_QUEUE_CAPACITY: usize = 1_024;
/// Most events one writer pass saves in a single store call.
const WRITE_BATCH_MAX: usize = 256;
/// Rows read from the store per query while a scoped history is filled.
const STORED_PAGE: usize = 2_000;
/// Random bytes behind the salt of the scope keys.
const SALT_BYTES: usize = 24;

/// The selection history of this process. Events get a sequence number when recorded.
#[derive(Debug)]
pub struct SelectionLog {
    salt: String,
    attempts: AtomicU64,
    state: Mutex<LogState>,
    writer: Option<SyncSender<SelectionEvent>>,
}

#[derive(Debug, Default)]
struct LogState {
    last_seq: u64,
    ring: VecDeque<SelectionEvent>,
    current: HashMap<String, WorkerCurrent>,
}

#[derive(Debug, Clone)]
struct WorkerCurrent {
    main: Option<SelectionEvent>,
    latest: SelectionEvent,
    active: Vec<SelectionEvent>,
}

impl SelectionLog {
    /// The log for `store`: the salt and the newest history are read from it, and a writer thread saves new events.
    /// Without a store, or when the store cannot be read, the log keeps events in memory only.
    pub fn open(store: Option<Arc<Store>>) -> SelectionLog {
        let Some(store) = store else {
            return Self::assemble(random_salt(), Vec::new(), None);
        };
        let salt = match store.install_salt(&random_salt()) {
            Ok(salt) => salt,
            Err(e) => {
                tracing::warn!(
                    "cannot read the selection salt. Selection history stays in memory: {e}"
                );
                return Self::assemble(random_salt(), Vec::new(), None);
            }
        };
        match store.load_selection_events(RING_CAPACITY) {
            Ok(history) => {
                let writer = spawn_writer(Arc::clone(&store));
                Self::assemble(salt, history, writer)
            }
            Err(e) => {
                tracing::warn!("cannot reload the selection history. It stays in memory: {e}");
                Self::assemble(salt, Vec::new(), None)
            }
        }
    }

    fn assemble(
        salt: String,
        history: Vec<SelectionEvent>,
        writer: Option<SyncSender<SelectionEvent>>,
    ) -> SelectionLog {
        let last_seq = history.iter().map(|event| event.seq).max().unwrap_or(0);
        let mut state = LogState {
            last_seq,
            ring: VecDeque::default(),
            current: HashMap::new(),
        };
        // Rebuild each worker's current selection from the reloaded history, so a restart keeps it.
        for event in &history {
            state.apply_current(event);
        }
        state.ring = history.into_iter().collect();
        SelectionLog {
            salt,
            attempts: AtomicU64::new(0),
            state: Mutex::new(state),
            writer,
        }
    }

    /// A salted SHA-256 of `raw` under `kind`, so a stored key never reveals the id it stands for.
    #[must_use]
    pub fn scope_key(&self, kind: ScopeKind, raw: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt.as_bytes());
        hasher.update([0u8]);
        hasher.update(kind.label().as_bytes());
        hasher.update([0u8]);
        hasher.update(raw.as_bytes());
        to_hex(&hasher.finalize())
    }

    /// A key that no other dispatch attempt in this process shares.
    #[must_use]
    pub fn attempt_key(&self) -> String {
        let ordinal = self.attempts.fetch_add(1, Ordering::Relaxed);
        self.scope_key(ScopeKind::Attempt, &ordinal.to_string())
    }

    /// Stamps `event` with the next sequence number and the current time, folds it into its worker's
    /// current selection, and queues it for saving. A full writer queue drops the save, never the event.
    pub fn record(&self, mut event: SelectionEvent) {
        event.ts_ms = now_ms();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.last_seq += 1;
        event.seq = state.last_seq;
        state.apply_current(&event);
        if state.ring.len() >= RING_CAPACITY {
            state.ring.pop_front();
        }
        if let Some(writer) = &self.writer
            && let Err(TrySendError::Full(_)) = writer.try_send(event.clone())
        {
            tracing::warn!("the selection writer queue is full. An event is not saved");
        }
        state.ring.push_back(event);
    }

    /// The events matching `query`, oldest first, so a client can page forward with `since_seq`.
    #[must_use]
    pub fn history(&self, query: &HistoryQuery) -> Vec<SelectionEvent> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .ring
            .iter()
            .filter(|event| {
                event.seq > query.since_seq
                    && matches_scope(query.workflow.as_deref(), event.workflow.as_deref())
                    && matches_scope(query.worker.as_deref(), event.worker.as_deref())
            })
            .take(query.limit)
            .cloned()
            .collect()
    }

    /// Events saved in `store` at or after `since_ts_ms` that match `query`, oldest first. This reads the saved
    /// history, which reaches back past the in-memory ring.
    pub fn stored_history(
        store: &Store,
        query: &HistoryQuery,
        since_ts_ms: u64,
    ) -> Result<Vec<SelectionEvent>, String> {
        let mut matched = Vec::new();
        let mut after_seq = query.since_seq;
        while matched.len() < query.limit {
            let page = store.load_selection_events_since(since_ts_ms, after_seq, STORED_PAGE)?;
            let Some(last) = page.last() else {
                break;
            };
            after_seq = last.seq;
            let full = page.len() == STORED_PAGE;
            matched.extend(page.into_iter().filter(|event| {
                matches_scope(query.workflow.as_deref(), event.workflow.as_deref())
                    && matches_scope(query.worker.as_deref(), event.worker.as_deref())
            }));
            if !full {
                break;
            }
        }
        matched.truncate(query.limit);
        Ok(matched)
    }

    /// The current selection of the only worker whose scope key starts with `prefix`; `None` when none or several match.
    #[must_use]
    pub fn current_by_prefix(&self, prefix: &str, now: u64) -> Option<CurrentSelection> {
        let key = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let mut matches = state.current.keys().filter(|key| key.starts_with(prefix));
            let only = matches.next()?.clone();
            if matches.next().is_some() {
                return None;
            }
            only
        };
        self.current(&key, now)
    }

    /// The current selection of the worker with scope key `worker`, or `None` before its first event.
    pub fn current(&self, worker: &str, now: u64) -> Option<CurrentSelection> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = state.current.get(worker)?;
        let age_ms = now.saturating_sub(entry.latest.ts_ms);
        Some(CurrentSelection {
            main: entry.main.clone(),
            active: entry.active.clone(),
            state: entry.latest.state,
            age_ms,
            stale: age_ms > CURRENT_STALE_AFTER_MS,
        })
    }
}

impl LogState {
    /// Folds `event` into its worker's current selection, evicting the least recently updated worker when full.
    fn apply_current(&mut self, event: &SelectionEvent) {
        let Some(worker) = event.worker.as_ref() else {
            return;
        };
        if !self.current.contains_key(worker) && self.current.len() >= CURRENT_CAPACITY {
            let oldest = self
                .current
                .iter()
                .min_by_key(|(_, entry)| entry.latest.seq)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.current.remove(&oldest);
            }
        }
        let entry = self
            .current
            .entry(worker.clone())
            .or_insert_with(|| WorkerCurrent {
                main: None,
                latest: event.clone(),
                active: Vec::new(),
            });
        entry.latest = event.clone();
        if event.stage == SelectionStage::Solver {
            entry.main = Some(event.clone());
        }
        let same_dispatch =
            |other: &SelectionEvent| other.stage == event.stage && other.attempt == event.attempt;
        match event.state {
            SelectionState::Selected | SelectionState::Started => {
                match entry.active.iter_mut().find(|other| same_dispatch(other)) {
                    Some(slot) => *slot = event.clone(),
                    None => entry.active.push(event.clone()),
                }
            }
            SelectionState::Completed | SelectionState::Rejected | SelectionState::Cancelled => {
                entry.active.retain(|other| !same_dispatch(other));
            }
        }
    }
}

fn matches_scope(wanted: Option<&str>, actual: Option<&str>) -> bool {
    wanted.is_none_or(|key| actual == Some(key))
}

fn random_salt() -> String {
    to_hex(&rand::random::<[u8; SALT_BYTES]>())
}

/// Starts the thread that saves queued events in batches. Returns `None` when the thread cannot start.
fn spawn_writer(store: Arc<Store>) -> Option<SyncSender<SelectionEvent>> {
    let (sender, receiver) = mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
    let spawned = thread::Builder::new()
        .name("selection-writer".to_owned())
        .spawn(move || write_batches(&store, &receiver));
    match spawned {
        Ok(_) => Some(sender),
        Err(e) => {
            tracing::warn!(
                "cannot start the selection writer. Selection history stays in memory: {e}"
            );
            None
        }
    }
}

fn write_batches(store: &Store, receiver: &Receiver<SelectionEvent>) {
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        while batch.len() < WRITE_BATCH_MAX {
            let Ok(next) = receiver.try_recv() else {
                break;
            };
            batch.push(next);
        }
        if let Err(e) = store.save_selection_events(&batch, now_ms(), SELECTION_RETENTION_EVENTS) {
            tracing::warn!("cannot save {} selection events: {e}", batch.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: &str = "worker-1";

    fn event(
        worker: Option<&String>,
        stage: SelectionStage,
        selection_state: SelectionState,
        attempt: &str,
    ) -> SelectionEvent {
        SelectionEvent {
            worker: worker.cloned(),
            attempt: Some(attempt.to_owned()),
            ..SelectionEvent::new(stage, selection_state, "copilot/m")
        }
    }

    fn history_of(log: &SelectionLog, worker: Option<&String>) -> Vec<SelectionEvent> {
        log.history(&HistoryQuery {
            worker: worker.cloned(),
            since_seq: 0,
            limit: usize::MAX,
            ..HistoryQuery::default()
        })
    }

    #[test]
    fn scope_keys_are_salted_and_separated_by_kind() {
        let log = SelectionLog::open(None);
        let other = SelectionLog::open(None);

        let worker = log.scope_key(ScopeKind::Worker, "session-1");

        assert_eq!(worker, log.scope_key(ScopeKind::Worker, "session-1"));
        assert_ne!(worker, log.scope_key(ScopeKind::Workflow, "session-1"));
        assert_ne!(worker, other.scope_key(ScopeKind::Worker, "session-1"));
        assert!(!worker.contains("session-1"));
    }

    #[test]
    fn sequence_numbers_are_monotonic_from_one() {
        let log = SelectionLog::open(None);
        for _ in 0..3 {
            log.record(event(
                None,
                SelectionStage::Solver,
                SelectionState::Selected,
                "a",
            ));
        }

        let seqs: Vec<u64> = history_of(&log, None).iter().map(|e| e.seq).collect();

        assert_eq!(seqs, [1, 2, 3]);
    }

    #[test]
    fn out_of_order_completion_keeps_the_other_stage_active() {
        let log = SelectionLog::open(None);
        let key = log.scope_key(ScopeKind::Worker, WORKER);
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Selected,
            "main",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Judge,
            SelectionState::Selected,
            "judge",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Judge,
            SelectionState::Completed,
            "judge",
        ));

        let current = log.current(&key, now_ms()).unwrap();

        assert_eq!(current.active.len(), 1);
        assert_eq!(current.active[0].attempt.as_deref(), Some("main"));
        assert_eq!(
            current.main.map(|e| e.state),
            Some(SelectionState::Selected)
        );
        assert_eq!(current.state, SelectionState::Completed);
    }

    #[test]
    fn finishing_the_solver_last_empties_the_active_stages() {
        let log = SelectionLog::open(None);
        let key = log.scope_key(ScopeKind::Worker, WORKER);
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Selected,
            "main",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Critic,
            SelectionState::Selected,
            "critic",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Critic,
            SelectionState::Started,
            "critic",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Started,
            "main",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Completed,
            "main",
        ));

        let current = log.current(&key, now_ms()).unwrap();

        assert_eq!(current.active.len(), 1);
        assert_eq!(current.active[0].stage, SelectionStage::Critic);
        assert_eq!(current.active[0].state, SelectionState::Started);
        assert_eq!(
            current.main.map(|e| e.state),
            Some(SelectionState::Completed)
        );
    }

    #[test]
    fn unrelated_workers_never_see_each_others_selection() {
        let log = SelectionLog::open(None);
        let first = log.scope_key(ScopeKind::Worker, "session-a");
        let second = log.scope_key(ScopeKind::Worker, "session-b");
        log.record(event(
            Some(&first),
            SelectionStage::Solver,
            SelectionState::Selected,
            "a",
        ));
        log.record(event(
            Some(&second),
            SelectionStage::Solver,
            SelectionState::Completed,
            "b",
        ));

        let current = log.current(&first, now_ms()).unwrap();

        assert_eq!(current.main.map(|e| e.attempt), Some(Some("a".to_owned())));
        assert_eq!(current.active.len(), 1);
        assert_eq!(history_of(&log, Some(&first)).len(), 1);
        assert_eq!(history_of(&log, Some(&second)).len(), 1);
        assert!(log.current("never-seen", now_ms()).is_none());
    }

    #[test]
    fn history_pages_forward_by_sequence_and_filters_by_scope() {
        let log = SelectionLog::open(None);
        let key = log.scope_key(ScopeKind::Worker, WORKER);
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Selected,
            "a",
        ));
        log.record(event(
            None,
            SelectionStage::Solver,
            SelectionState::Selected,
            "b",
        ));
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Completed,
            "a",
        ));

        let page = log.history(&HistoryQuery {
            worker: Some(key.clone()),
            since_seq: 1,
            limit: 10,
            ..HistoryQuery::default()
        });

        assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), [3]);
        assert_eq!(history_of(&log, None).len(), 3);
    }

    #[test]
    fn stale_marks_a_current_selection_older_than_the_threshold() {
        let log = SelectionLog::open(None);
        let key = log.scope_key(ScopeKind::Worker, WORKER);
        log.record(event(
            Some(&key),
            SelectionStage::Solver,
            SelectionState::Selected,
            "a",
        ));
        let recorded = log.current(&key, now_ms()).unwrap();
        let later = log
            .current(&key, now_ms() + CURRENT_STALE_AFTER_MS + 1_000)
            .unwrap();

        assert!(!recorded.stale);
        assert!(later.stale);
        assert!(later.age_ms >= CURRENT_STALE_AFTER_MS);
    }

    #[test]
    fn ring_and_worker_map_stay_bounded() {
        let log = SelectionLog::open(None);
        for index in 0..=CURRENT_CAPACITY {
            let key = log.scope_key(ScopeKind::Worker, &index.to_string());
            log.record(event(
                Some(&key),
                SelectionStage::Solver,
                SelectionState::Selected,
                "a",
            ));
        }
        for _ in 0..RING_CAPACITY {
            log.record(event(
                None,
                SelectionStage::Solver,
                SelectionState::Selected,
                "b",
            ));
        }

        let state = log.state.lock().unwrap();

        assert_eq!(state.ring.len(), RING_CAPACITY);
        assert_eq!(state.current.len(), CURRENT_CAPACITY);
        assert_eq!(
            state.ring.front().map(|e| e.seq),
            Some(state.last_seq + 1 - RING_CAPACITY as u64)
        );
    }

    #[test]
    fn history_and_salt_survive_a_restart_but_affinity_state_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let path_str = path.to_str().unwrap();
        let key = {
            let store = Arc::new(Store::open("sqlite", Some(path_str)).unwrap());
            let log = SelectionLog::open(Some(store.clone()));
            let key = log.scope_key(ScopeKind::Worker, WORKER);
            log.record(event(
                Some(&key),
                SelectionStage::Solver,
                SelectionState::Selected,
                "a",
            ));
            log.record(event(
                Some(&key),
                SelectionStage::Solver,
                SelectionState::Completed,
                "a",
            ));
            wait_for_saved(&store, 2);
            key
        };
        let store = Arc::new(Store::open("sqlite", Some(path_str)).unwrap());

        let log = SelectionLog::open(Some(store));
        let reloaded = history_of(&log, None);

        assert_eq!(reloaded.len(), 2);
        assert_eq!(log.scope_key(ScopeKind::Worker, WORKER), key);
        // The current selection is rebuilt from the saved history, so it survives the restart.
        assert!(log.current(&key, now_ms()).is_some());
        log.record(event(
            None,
            SelectionStage::Solver,
            SelectionState::Selected,
            "b",
        ));
        assert_eq!(history_of(&log, None).last().map(|e| e.seq), Some(3));
    }

    fn wait_for_saved(store: &Store, count: usize) {
        for _ in 0..500 {
            if store.load_selection_events(count).unwrap().len() >= count {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the writer did not save {count} selection events");
    }
}

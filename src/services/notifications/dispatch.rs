//! The one entry point every notification source uses: inbox, mutes, severity filter and delivery
//! to the sinks through a bounded queue and one background task.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::future::BoxFuture;
use tokio::sync::mpsc;

use crate::configs::NotifyConfig;
use crate::schemas::notification::{
    MuteOrigin, MuteScope, Notification, NotificationMute, NotificationStatus,
};
use crate::services::error_signature::error_signature;
use crate::services::notifications::command_sink::CommandSink;
use crate::services::notifications::desktop::DesktopSink;
use crate::services::notifications::inbox::{Inbox, Upserted};
use crate::services::notifications::mute::{MuteError, MuteSet};
use crate::services::notifications::sources::{ErrorWatch, HealthWatch};
use crate::services::notifications::webhook::WebhookSink;
use crate::services::stats::RequestOutcome;
use crate::services::store::{Store, now_ms};

/// How many notifications wait for delivery before new ones are dropped.
pub const QUEUE_CAP: usize = 256;

/// A delivery target such as the desktop, a webhook or a command.
pub trait Sink: Send + Sync {
    /// A short name for logs.
    fn name(&self) -> &'static str;
    /// Delivers `notification`, or returns why it could not.
    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>>;
}

impl<S: Sink + ?Sized> Sink for Arc<S> {
    fn name(&self) -> &'static str {
        (**self).name()
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
        (**self).send(notification)
    }
}

impl Sink for DesktopSink {
    fn name(&self) -> &'static str {
        "desktop"
    }

    /// Skips the popup when `LLM_HUB_NOTIFY_DESKTOP` turned the sink off.
    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if !self.enabled() {
                return Ok(());
            }
            DesktopSink::send(self, notification).await
        })
    }
}

impl Sink for WebhookSink {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(WebhookSink::send(self, notification))
    }
}

impl Sink for CommandSink {
    fn name(&self) -> &'static str {
        "command"
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(CommandSink::send(self, notification))
    }
}

/// Holds the inbox and the mute rules, and hands new notifications to the delivery task.
pub struct Notifier {
    config: ArcSwap<NotifyConfig>,
    store: Option<Arc<Store>>,
    inbox: Arc<Inbox>,
    /// Held while a store job reads the inbox or the mutes and writes the store, so the last
    /// job leaves the live state in the store.
    save_lock: Arc<Mutex<()>>,
    mutes: Arc<RwLock<MuteSet>>,
    queue: OnceLock<mpsc::Sender<Notification>>,
    dropped: AtomicU64,
    /// The desktop sink the delivery task uses, shared with the desktop setup routes.
    desktop: OnceLock<Arc<DesktopSink>>,
    /// The error groups seen so far, so only a new group notifies.
    errors: ErrorWatch,
    /// The profiles last reported unhealthy, so only a health flip notifies.
    health: HealthWatch,
}

impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notifier")
            .field("config", &self.config.load())
            .field("persisted", &self.store.is_some())
            .field("inbox", &self.inbox)
            .field("mutes", &self.read_mutes().list().len())
            .field("delivering", &self.queue.get().is_some())
            .field("dropped", &self.dropped())
            .field("desktop", &self.desktop.get().is_some())
            .finish_non_exhaustive()
    }
}

impl Notifier {
    /// A notifier with the saved notifications and UI mute rules of `store`, plus the env mutes of
    /// `config`. Saved notifications past the inbox bound are deleted from the store off the
    /// caller's task. A failed load logs and starts empty.
    #[must_use]
    pub fn new(config: &NotifyConfig, store: Option<Arc<Store>>) -> Arc<Notifier> {
        let (inbox, evicted) = Inbox::load(store.as_deref());
        let inbox = Arc::new(inbox);
        let ui = match store.as_deref().map(Store::load_notification_mutes) {
            Some(Ok(rules)) => rules,
            Some(Err(error)) => {
                tracing::warn!("cannot load saved notification mutes: {error}");
                Vec::new()
            }
            None => Vec::new(),
        };
        let notifier = Arc::new(Notifier {
            config: ArcSwap::from_pointee(config.clone()),
            store,
            inbox,
            save_lock: Arc::new(Mutex::new(())),
            mutes: Arc::new(RwLock::new(MuteSet::new(config.mutes.clone(), ui))),
            queue: OnceLock::new(),
            dropped: AtomicU64::new(0),
            desktop: OnceLock::new(),
            errors: ErrorWatch::default(),
            health: HealthWatch::default(),
        });
        notifier.delete_evicted(evicted);
        notifier.seed_errors();
        notifier
    }

    /// Notifies the first failure of an error group. Memory only, so the request path may call it.
    pub fn note_error(&self, outcome: &RequestOutcome) {
        if !self.config.load().enabled {
            return;
        }
        if let Some(notification) = self.errors.observe(outcome, now_ms()) {
            self.notify(notification);
        }
    }

    /// Notifies a health flip of `profile`: unhealthy, or healthy again after it was unhealthy.
    pub fn note_health(&self, profile: &str, healthy: bool, reason: &str) {
        if let Some(notification) = self.health.observe(profile, healthy, reason, now_ms()) {
            self.notify(notification);
        }
    }

    /// Marks the error groups already in the store as seen, off the caller's task, then notifies
    /// the new groups that failed meanwhile.
    fn seed_errors(self: &Arc<Self>) {
        if self.store.is_none() {
            self.errors.seed(Vec::new());
            return;
        }
        let notifier = Arc::downgrade(self);
        self.with_store(move |store| {
            let signatures: Vec<String> = match store.error_tallies(None) {
                Ok(tallies) => tallies
                    .iter()
                    .map(|tally| error_signature(tally.error.as_deref(), tally.status))
                    .collect(),
                Err(error) => {
                    tracing::warn!("cannot read the stored error groups: {error}");
                    Vec::new()
                }
            };
            let Some(notifier) = notifier.upgrade() else {
                return;
            };
            for notification in notifier.errors.seed(signatures) {
                notifier.notify(notification);
            }
        });
    }

    /// Starts the one task that delivers queued notifications to `sinks`, in order. A second call
    /// logs and returns `None`.
    pub fn spawn_delivery(
        self: &Arc<Self>,
        sinks: Vec<Box<dyn Sink>>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let Some(mut rx) = self.attach(QUEUE_CAP) else {
            tracing::warn!("notification delivery already runs");
            return None;
        };
        Some(tokio::spawn(async move {
            while let Some(notification) = rx.recv().await {
                for sink in &sinks {
                    if let Err(error) = sink.send(&notification).await {
                        tracing::warn!(
                            "notification sink {} failed for {}: {error}",
                            sink.name(),
                            notification.id
                        );
                    }
                }
            }
        }))
    }

    /// Shares `sink` with the desktop setup routes. Pass the same sink to [`Self::spawn_delivery`].
    /// A second call keeps the first sink and returns false.
    pub fn set_desktop(&self, sink: Arc<DesktopSink>) -> bool {
        self.desktop.set(sink).is_ok()
    }

    /// The desktop sink set by [`Self::set_desktop`], or `None` before startup sets it.
    #[must_use]
    pub fn desktop(&self) -> Option<Arc<DesktopSink>> {
        self.desktop.get().map(Arc::clone)
    }

    /// Takes the new config: the on switch, the severity filter and the env mutes.
    pub fn reconfigure(&self, config: &NotifyConfig) {
        let ui: Vec<NotificationMute> = self
            .read_mutes()
            .list()
            .into_iter()
            .filter(|rule| rule.origin == MuteOrigin::Ui)
            .collect();
        *self.write_mutes() = MuteSet::new(config.mutes.clone(), ui);
        self.config.store(Arc::new(config.clone()));
    }

    /// Stores `notification` and queues a new one for the sinks. Returns the stored notification,
    /// or `None` when notifications are off.
    ///
    /// A mute with scope `all` stores it as muted; any mute, or a severity under the minimum,
    /// skips the sinks. A known id keeps its status and is never delivered again.
    pub fn notify(&self, notification: Notification) -> Option<Notification> {
        if !self.config.load().enabled {
            return None;
        }
        let (notification, scope) = self.apply_mutes(notification);
        let upserted = self.inbox.upsert(notification.clone());
        Some(self.after_upsert(notification, scope, upserted))
    }

    /// [`Self::notify`] for a guard event, skipped when the inbox saw one with the same source,
    /// profile, model and rule within the dedupe window. Returns `None` when off or skipped.
    pub fn notify_guard(&self, notification: Notification) -> Option<Notification> {
        if !self.config.load().enabled {
            return None;
        }
        let (notification, scope) = self.apply_mutes(notification);
        let upserted = self.inbox.note_guard(notification.clone(), now_ms())?;
        Some(self.after_upsert(notification, scope, upserted))
    }

    /// Every notification, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<Notification> {
        self.inbox.list()
    }

    /// The notification `id`, when the inbox holds it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Notification> {
        self.inbox.get(id)
    }

    /// Sets the status and result of `id`, saves it and returns it, or `None` for an unknown id.
    pub fn set_status(
        &self,
        id: &str,
        status: NotificationStatus,
        result: Option<String>,
    ) -> Option<Notification> {
        let updated = self.inbox.set_status(id, status, result)?;
        self.save(id);
        Some(updated)
    }

    /// How many pending notifications are not muted.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.inbox.pending_count()
    }

    /// How many notifications the full queue dropped since start.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Every mute rule, env rules first.
    #[must_use]
    pub fn mutes(&self) -> Vec<NotificationMute> {
        self.read_mutes().list()
    }

    /// Adds or replaces the UI rule `rule` and saves it.
    ///
    /// # Errors
    /// [`MuteError`] when the set refuses the rule.
    pub fn add_mute(&self, rule: NotificationMute) -> Result<NotificationMute, MuteError> {
        let rule = NotificationMute {
            origin: MuteOrigin::Ui,
            ..rule
        };
        self.write_mutes().add_ui(rule.clone())?;
        self.persist_mutes(vec![rule.id.clone()]);
        Ok(rule)
    }

    /// Removes the UI rule `id` and deletes it from the store. Returns false for an unknown id.
    ///
    /// # Errors
    /// [`MuteError::EnvRule`] when `id` names an env rule.
    pub fn remove_mute(&self, id: &str) -> Result<bool, MuteError> {
        let removed = self.write_mutes().remove(id)?;
        if removed {
            self.persist_mutes(vec![id.to_owned()]);
        }
        Ok(removed)
    }

    /// Drops the snoozes that ended by `now_ms`, deletes them from the store and returns their ids.
    pub fn prune_tick(&self, now_ms: u64) -> Vec<String> {
        let expired = self.write_mutes().prune_expired(now_ms);
        if !expired.is_empty() {
            self.persist_mutes(expired.clone());
        }
        expired
    }

    /// Runs [`Self::prune_tick`] every `every`, for as long as the notifier lives.
    pub fn spawn_prune_loop(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let notifier = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(every);
            ticks.tick().await;
            loop {
                ticks.tick().await;
                let Some(notifier) = notifier.upgrade() else {
                    return;
                };
                notifier.prune_tick(now_ms());
            }
        })
    }

    fn apply_mutes(&self, mut notification: Notification) -> (Notification, Option<MuteScope>) {
        let scope = self.read_mutes().matches(&notification, now_ms());
        notification.muted = scope == Some(MuteScope::All);
        (notification, scope)
    }

    /// Saves what the inbox now holds for `notification`, deletes the evicted one from the store,
    /// and queues `notification` when new and not filtered.
    fn after_upsert(
        &self,
        notification: Notification,
        scope: Option<MuteScope>,
        upserted: Upserted,
    ) -> Notification {
        let Upserted { is_new, evicted } = upserted;
        let stored = self
            .inbox
            .get(&notification.id)
            .unwrap_or_else(|| notification.clone());
        if is_new || stored.status == NotificationStatus::Pending {
            self.save(&stored.id);
        }
        if let Some(id) = evicted {
            self.delete_evicted(vec![id]);
        }
        if is_new && scope.is_none() && notification.severity >= self.config.load().min_severity {
            self.enqueue(notification);
        }
        stored
    }

    fn enqueue(&self, notification: Notification) {
        let Some(queue) = self.queue.get() else {
            return;
        };
        match queue.try_send(notification) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(notification)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "notification queue is full; {} is not delivered",
                    notification.id
                );
            }
            Err(mpsc::error::TrySendError::Closed(notification)) => {
                tracing::debug!(
                    "notification delivery stopped; {} is not delivered",
                    notification.id
                );
            }
        }
    }

    /// Opens the delivery queue with room for `capacity` and returns its receiver, once.
    fn attach(&self, capacity: usize) -> Option<mpsc::Receiver<Notification>> {
        let (tx, rx) = mpsc::channel(capacity);
        self.queue.set(tx).ok()?;
        Some(rx)
    }

    /// Saves what the inbox holds for `id` when the save runs, so saves that run out of order
    /// still leave the latest state in the store.
    fn save(&self, id: &str) {
        let inbox = Arc::clone(&self.inbox);
        let save_lock = Arc::clone(&self.save_lock);
        let id = id.to_owned();
        self.with_store(move |store| {
            let _order = save_lock.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(latest) = inbox.get(&id) else {
                return;
            };
            if let Err(error) = store.save_notification(&latest) {
                tracing::warn!("cannot save notification {id}: {error}");
            }
        });
    }

    /// Deletes the evicted notifications `ids` from the store, skipping any the inbox holds again
    /// when the job runs.
    fn delete_evicted(&self, ids: Vec<String>) {
        if ids.is_empty() {
            return;
        }
        let inbox = Arc::clone(&self.inbox);
        let save_lock = Arc::clone(&self.save_lock);
        self.with_store(move |store| {
            let _order = save_lock.lock().unwrap_or_else(PoisonError::into_inner);
            let gone: Vec<String> = ids
                .into_iter()
                .filter(|id| inbox.get(id).is_none())
                .collect();
            if let Err(error) = store.delete_notifications(&gone) {
                tracing::warn!(
                    "cannot delete {} evicted notifications: {error}",
                    gone.len()
                );
            }
        });
    }

    /// Writes the live state of the UI mute rules `ids` to the store when the job runs, so jobs
    /// that run out of order still leave the latest rules in the store.
    fn persist_mutes(&self, ids: Vec<String>) {
        let mutes = Arc::clone(&self.mutes);
        let save_lock = Arc::clone(&self.save_lock);
        self.with_store(move |store| {
            let _order = save_lock.lock().unwrap_or_else(PoisonError::into_inner);
            let live: Vec<(String, Option<NotificationMute>)> = {
                let set = mutes.read().unwrap_or_else(PoisonError::into_inner);
                ids.into_iter()
                    .map(|id| {
                        let rule = set.ui_rule(&id).cloned();
                        (id, rule)
                    })
                    .collect()
            };
            sync_mutes(store, &live);
        });
    }

    /// Runs `job` on the store off the caller's task, or inline outside a runtime.
    fn with_store(&self, job: impl FnOnce(&Store) + Send + 'static) {
        let Some(store) = self.store.as_ref().map(Arc::clone) else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(move || job(&store));
        } else {
            job(&store);
        }
    }

    fn read_mutes(&self) -> std::sync::RwLockReadGuard<'_, MuteSet> {
        self.mutes.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_mutes(&self) -> std::sync::RwLockWriteGuard<'_, MuteSet> {
        self.mutes.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Saves each live rule of `live` to `store` and deletes each id whose rule is gone.
fn sync_mutes(store: &Store, live: &[(String, Option<NotificationMute>)]) {
    for (id, rule) in live {
        let result = match rule {
            Some(rule) => store.save_notification_mute(rule),
            None => store.delete_notification_mute(id).map(drop),
        };
        if let Err(error) = result {
            tracing::warn!("cannot store notification mute {id}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::schemas::notification::{MuteMatch, NotificationKind, NotificationSeverity};
    use crate::services::notifications::inbox::INBOX_CAP;

    fn notification(id: &str, severity: NotificationSeverity) -> Notification {
        Notification {
            id: id.into(),
            kind: NotificationKind::Custom,
            source: "api".into(),
            severity,
            title: "Disk almost full".into(),
            body: "body".into(),
            profile: None,
            model: None,
            rule: None,
            diff: None,
            actions: Notification::default_actions(NotificationKind::Custom),
            status: NotificationStatus::Pending,
            created_ms: 1,
            updated_ms: 1,
            result: None,
            muted: false,
        }
    }

    fn warning(id: &str) -> Notification {
        notification(id, NotificationSeverity::Warning)
    }

    fn json_store(dir: &tempfile::TempDir) -> Arc<Store> {
        let path = dir.path().join("store.json");
        Arc::new(Store::open("json", path.to_str()).unwrap())
    }

    fn mute(id: &str, source: &str, scope: MuteScope, until_ms: Option<u64>) -> NotificationMute {
        NotificationMute {
            id: id.into(),
            matcher: MuteMatch {
                source: Some(source.into()),
                ..MuteMatch::default()
            },
            until_ms,
            scope,
            note: None,
            origin: MuteOrigin::Env,
            created_ms: 1,
        }
    }

    /// Records every notification it gets, or fails each send when `fail` is set.
    #[derive(Default)]
    struct FakeSink {
        seen: Mutex<Vec<String>>,
        fail: bool,
    }

    impl Sink for FakeSink {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(notification.id.clone());
                if self.fail {
                    Err("boom".into())
                } else {
                    Ok(())
                }
            })
        }
    }

    fn queued(rx: &mut mpsc::Receiver<Notification>) -> Vec<String> {
        let mut ids = Vec::new();
        while let Ok(n) = rx.try_recv() {
            ids.push(n.id);
        }
        ids
    }

    #[tokio::test]
    async fn delivers_to_every_sink_even_after_one_fails() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let failing = Arc::new(FakeSink {
            fail: true,
            ..FakeSink::default()
        });
        let ok = Arc::new(FakeSink::default());
        let handle = notifier
            .spawn_delivery(vec![
                Box::new(Arc::clone(&failing)),
                Box::new(Arc::clone(&ok)),
            ])
            .unwrap();
        assert!(notifier.spawn_delivery(Vec::new()).is_none());
        notifier.notify(warning("a"));
        for _ in 0..100 {
            if !ok.seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(*failing.seen.lock().unwrap(), ["a"]);
        assert_eq!(*ok.seen.lock().unwrap(), ["a"]);
        assert_eq!(Arc::clone(&ok).name(), "fake");
        handle.abort();
    }

    #[tokio::test]
    async fn severity_filters_delivery_but_not_the_inbox() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        notifier.notify(notification("info", NotificationSeverity::Info));
        notifier.notify(notification("crit", NotificationSeverity::Critical));
        assert_eq!(queued(&mut rx), ["crit"]);
        assert_eq!(notifier.list().len(), 2);
        assert_eq!(notifier.pending_count(), 2);

        let config = NotifyConfig {
            min_severity: NotificationSeverity::Info,
            ..NotifyConfig::default()
        };
        notifier.reconfigure(&config);
        notifier.notify(notification("info2", NotificationSeverity::Info));
        assert_eq!(queued(&mut rx), ["info2"]);
    }

    #[tokio::test]
    async fn disabled_stores_and_sends_nothing() {
        let config = NotifyConfig {
            enabled: false,
            ..NotifyConfig::default()
        };
        let notifier = Notifier::new(&config, None);
        let mut rx = notifier.attach(8).unwrap();
        assert!(notifier.notify(warning("a")).is_none());
        assert!(notifier.notify_guard(warning("g")).is_none());
        assert!(notifier.list().is_empty());
        assert!(queued(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn env_mute_all_stores_muted_and_skips_sinks() {
        let config = NotifyConfig {
            mutes: vec![MuteMatch {
                source: Some("api".into()),
                ..MuteMatch::default()
            }],
            ..NotifyConfig::default()
        };
        let notifier = Notifier::new(&config, None);
        let mut rx = notifier.attach(8).unwrap();
        let stored = notifier.notify(warning("a")).unwrap();
        assert!(stored.muted);
        assert!(notifier.get("a").unwrap().muted);
        assert_eq!(notifier.pending_count(), 0);
        assert!(queued(&mut rx).is_empty());
        assert_eq!(notifier.mutes()[0].origin, MuteOrigin::Env);
    }

    #[tokio::test]
    async fn sinks_mute_shows_in_the_inbox_without_delivery() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        notifier
            .add_mute(mute("m", "api", MuteScope::Sinks, None))
            .unwrap();
        let stored = notifier.notify(warning("a")).unwrap();
        assert!(!stored.muted);
        assert_eq!(notifier.pending_count(), 1);
        assert!(queued(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn a_known_id_is_not_delivered_again() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        notifier.notify(warning("a"));
        let mut again = warning("a");
        again.body = "newer".into();
        let stored = notifier.notify(again).unwrap();
        assert_eq!(stored.body, "newer");
        assert_eq!(queued(&mut rx), ["a"]);
    }

    #[tokio::test]
    async fn a_full_queue_drops_without_blocking() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(1).unwrap();
        notifier.notify(warning("a"));
        notifier.notify(warning("b"));
        assert_eq!(notifier.dropped(), 1);
        assert_eq!(queued(&mut rx), ["a"]);
        assert!(notifier.get("b").is_some());
        drop(rx);
        notifier.notify(warning("c"));
        assert_eq!(notifier.dropped(), 1);
    }

    #[tokio::test]
    async fn notify_guard_dedupes_and_delivers_the_first() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        assert!(notifier.notify_guard(warning("g1")).is_some());
        assert!(notifier.notify_guard(warning("g2")).is_none());
        assert_eq!(queued(&mut rx), ["g1"]);
    }

    #[tokio::test]
    async fn notify_before_delivery_starts_only_stores() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        assert!(notifier.notify(warning("a")).is_some());
        assert_eq!(notifier.list().len(), 1);
    }

    fn failure(error: &str) -> RequestOutcome {
        RequestOutcome {
            profile: "local".into(),
            model_key: "local/gemma".into(),
            status: 500,
            error: Some(error.into()),
            ..RequestOutcome::default()
        }
    }

    #[tokio::test]
    async fn a_new_error_group_notifies_once() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        notifier.note_error(&failure("boom 12345"));
        notifier.note_error(&failure("boom 67890"));
        let delivered = queued(&mut rx);
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].starts_with("errors:"));
        assert_eq!(notifier.list().len(), 1);
        assert_eq!(notifier.list()[0].title, "New error: boom <n>");
    }

    #[tokio::test]
    async fn note_error_does_nothing_when_off() {
        let config = NotifyConfig {
            enabled: false,
            ..NotifyConfig::default()
        };
        let notifier = Notifier::new(&config, None);
        notifier.note_error(&failure("boom"));
        assert!(notifier.list().is_empty());
    }

    #[test]
    fn stored_error_groups_never_notify_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        store.record(&failure("old boom")).unwrap();
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier.note_error(&failure("old boom"));
        assert!(notifier.list().is_empty());
        notifier.note_error(&failure("new boom"));
        assert_eq!(notifier.list()[0].title, "New error: new boom");
    }

    #[tokio::test]
    async fn the_background_seed_sends_groups_held_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        store.record(&failure("old boom")).unwrap();
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier.note_error(&failure("new boom"));
        for _ in 0..100 {
            if !notifier.list().is_empty() {
                notifier.note_error(&failure("old boom"));
                notifier.note_error(&failure("new boom"));
                assert_eq!(notifier.list().len(), 1);
                assert_eq!(notifier.list()[0].title, "New error: new boom");
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the seed never sent the held group");
    }

    #[tokio::test]
    async fn a_health_flip_notifies_once_per_flip() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let mut rx = notifier.attach(8).unwrap();
        notifier.note_health("local", true, "up");
        notifier.note_health("local", false, "refused");
        notifier.note_health("local", false, "refused");
        notifier.note_health("local", true, "up");
        notifier.note_health("local", true, "up");
        let delivered = queued(&mut rx);
        // The default minimum severity is warning, so the recovery stays in the inbox only.
        assert_eq!(delivered.len(), 1);
        assert!(delivered[0].starts_with("health:local:unhealthy:"));
        let listed = notifier.list();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .any(|n| n.id.starts_with("health:local:recovered:"))
        );
    }

    #[tokio::test]
    async fn set_status_saves_and_reports_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier.notify(warning("a"));
        let updated = notifier
            .set_status("a", NotificationStatus::Dismissed, None)
            .unwrap();
        assert_eq!(updated.status, NotificationStatus::Dismissed);
        assert!(
            notifier
                .set_status("x", NotificationStatus::Dismissed, None)
                .is_none()
        );
        for _ in 0..100 {
            let saved = store.load_notifications().unwrap();
            if saved
                .first()
                .is_some_and(|n| n.status == NotificationStatus::Dismissed)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the dismissed status was never saved");
    }

    #[test]
    fn outside_a_runtime_saves_inline_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier.notify(warning("a"));
        notifier
            .add_mute(mute("m", "errors", MuteScope::All, None))
            .unwrap();
        assert_eq!(store.load_notifications().unwrap()[0].id, "a");
        let saved = store.load_notification_mutes().unwrap();
        assert_eq!(saved[0].origin, MuteOrigin::Ui);

        let reloaded = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        assert_eq!(reloaded.get("a").unwrap().id, "a");
        assert_eq!(reloaded.mutes()[0].id, "m");
        assert!(format!("{reloaded:?}").contains("Notifier"));

        assert_eq!(reloaded.remove_mute("m"), Ok(true));
        assert_eq!(reloaded.remove_mute("m"), Ok(false));
        assert!(store.load_notification_mutes().unwrap().is_empty());
    }

    #[test]
    fn env_mutes_are_read_only_and_survive_reconfigure_with_ui_rules() {
        let config = NotifyConfig {
            mutes: vec![MuteMatch {
                source: Some("errors".into()),
                ..MuteMatch::default()
            }],
            ..NotifyConfig::default()
        };
        let notifier = Notifier::new(&config, None);
        assert_eq!(
            notifier.remove_mute("env-0"),
            Err(MuteError::EnvRule("env-0".into()))
        );
        notifier
            .add_mute(mute("u", "api", MuteScope::Sinks, None))
            .unwrap();
        notifier.reconfigure(&NotifyConfig::default());
        let ids: Vec<_> = notifier.mutes().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["u"]);
    }

    #[test]
    fn a_late_mute_job_writes_the_live_rule_not_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        let rule = NotificationMute {
            origin: MuteOrigin::Ui,
            ..mute("m", "api", MuteScope::All, None)
        };
        // The add mutates the set, but its store job runs only after the remove's job.
        notifier.write_mutes().add_ui(rule.clone()).unwrap();
        assert_eq!(notifier.remove_mute("m"), Ok(true));
        notifier.persist_mutes(vec!["m".into()]);
        assert!(store.load_notification_mutes().unwrap().is_empty());

        // A late job for a rule the set still holds saves it.
        notifier.write_mutes().add_ui(rule.clone()).unwrap();
        notifier.persist_mutes(vec!["m".into()]);
        assert_eq!(store.load_notification_mutes().unwrap(), [rule]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mute_jobs_in_a_runtime_leave_the_removed_rule_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        for _ in 0..20 {
            notifier
                .add_mute(mute("m", "api", MuteScope::All, None))
                .unwrap();
            assert_eq!(notifier.remove_mute("m"), Ok(true));
        }
        notifier
            .add_mute(mute("sentinel", "api", MuteScope::All, None))
            .unwrap();
        for _ in 0..200 {
            let saved = store.load_notification_mutes().unwrap();
            if saved.iter().any(|m| m.id == "sentinel") {
                let _jobs = notifier.save_lock.lock().unwrap();
                let saved = store.load_notification_mutes().unwrap();
                assert!(saved.iter().all(|m| m.id != "m"), "{saved:?}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the mute jobs never ran");
    }

    #[test]
    fn startup_deletes_saved_notifications_past_the_inbox_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        for index in 0..=INBOX_CAP {
            let mut saved = warning(&format!("n{index}"));
            saved.created_ms = index as u64 + 1;
            store.save_notification(&saved).unwrap();
        }
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        assert_eq!(notifier.list().len(), INBOX_CAP);
        let saved = store.load_notifications().unwrap();
        assert_eq!(saved.len(), INBOX_CAP);
        assert!(saved.iter().all(|n| n.id != "n0"));
    }

    #[test]
    fn an_eviction_deletes_the_evicted_notification_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        for index in 0..INBOX_CAP {
            let mut item = warning(&format!("n{index}"));
            item.created_ms = index as u64 + 1;
            notifier.notify(item);
        }
        notifier.set_status("n3", NotificationStatus::Dismissed, None);
        let mut newest = warning("newest");
        newest.created_ms = 10_000;
        notifier.notify(newest);
        assert!(notifier.get("n3").is_none());
        let saved = store.load_notifications().unwrap();
        assert_eq!(saved.len(), INBOX_CAP);
        assert!(saved.iter().all(|n| n.id != "n3"));
        assert!(saved.iter().any(|n| n.id == "newest"));
    }

    #[test]
    fn an_evicted_id_back_in_the_inbox_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier.notify(warning("a"));
        notifier.delete_evicted(vec!["a".into()]);
        assert_eq!(store.load_notifications().unwrap()[0].id, "a");
        notifier.delete_evicted(Vec::new());
    }

    #[test]
    fn prune_tick_removes_expired_snoozes_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = json_store(&dir);
        let notifier = Notifier::new(&NotifyConfig::default(), Some(Arc::clone(&store)));
        notifier
            .add_mute(mute("snooze", "api", MuteScope::All, Some(100)))
            .unwrap();
        notifier
            .add_mute(mute("forever", "api", MuteScope::All, None))
            .unwrap();
        assert!(notifier.prune_tick(99).is_empty());
        assert_eq!(notifier.prune_tick(100), ["snooze"]);
        let saved = store.load_notification_mutes().unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, "forever");
    }

    #[tokio::test]
    async fn the_prune_loop_stops_when_the_notifier_drops() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let handle = notifier.spawn_prune_loop(Duration::from_millis(1));
        tokio::time::sleep(Duration::from_millis(5)).await;
        drop(notifier);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn the_desktop_sink_is_set_once_and_shared() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        assert!(notifier.desktop().is_none());
        let first = Arc::new(DesktopSink::new(true, Duration::from_secs(1)));
        assert!(notifier.set_desktop(Arc::clone(&first)));
        assert!(!notifier.set_desktop(Arc::new(DesktopSink::new(false, Duration::from_secs(1)))));
        assert!(Arc::ptr_eq(&notifier.desktop().unwrap(), &first));
        assert!(format!("{notifier:?}").contains("desktop: true"));
    }

    #[test]
    fn concrete_sinks_name_themselves() {
        let desktop = DesktopSink::new(false, Duration::from_secs(1));
        let webhook = WebhookSink::new(
            reqwest::Client::new(),
            "http://127.0.0.1:9/x".into(),
            Duration::from_millis(50),
        );
        let command = CommandSink::new(vec!["true".into()], Duration::from_secs(1));
        assert_eq!(Sink::name(&desktop), "desktop");
        assert_eq!(Sink::name(&webhook), "webhook");
        assert_eq!(Sink::name(&command), "command");
    }

    #[tokio::test]
    async fn desktop_sink_skips_when_off_and_reports_an_unsupported_os() {
        use crate::services::notifications::desktop::{DesktopOs, SystemRunner};
        let sink = |enabled| {
            DesktopSink::with_parts(
                enabled,
                DesktopOs::Other,
                Arc::new(SystemRunner),
                Duration::from_secs(1),
                |_| None,
            )
        };
        assert_eq!(Sink::send(&sink(false), &warning("a")).await, Ok(()));
        assert!(Sink::send(&sink(true), &warning("a")).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_sink_forwards_send() {
        let command = CommandSink::new(
            vec!["sh".into(), "-c".into(), "cat >/dev/null".into()],
            Duration::from_secs(5),
        );
        assert_eq!(Sink::send(&command, &warning("a")).await, Ok(()));
    }

    #[tokio::test]
    async fn webhook_sink_forwards_send() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let webhook = WebhookSink::new(
            reqwest::Client::new(),
            format!("{}/hook", server.uri()),
            Duration::from_secs(5),
        );
        assert_eq!(Sink::send(&webhook, &warning("a")).await, Ok(()));
    }
}

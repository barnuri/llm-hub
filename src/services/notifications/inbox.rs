//! The in-memory notifications inbox and its persistence glue.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::schemas::notification::{Notification, NotificationStatus};
use crate::services::store::{Store, now_ms};

/// The most notifications the inbox keeps.
pub const INBOX_CAP: usize = 500;
/// How long a guard event blocks another one with the same source, profile, model and rule.
pub const GUARD_DEDUPE_MS: u64 = 10 * 60 * 1000;

type GuardKey = (String, Option<String>, Option<String>, Option<String>);

/// What [`Inbox::upsert`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Upserted {
    /// True when the id was new to the inbox.
    pub is_new: bool,
    /// The id the [`INBOX_CAP`] bound evicted to make room, which the store should delete too.
    pub evicted: Option<String>,
}

#[derive(Debug, Default)]
struct InboxState {
    items: Vec<Notification>,
    guards: HashMap<GuardKey, u64>,
}

/// Every notification the hub holds, bounded at [`INBOX_CAP`].
#[derive(Debug, Default)]
pub struct Inbox {
    state: Mutex<InboxState>,
}

impl Inbox {
    /// An inbox holding the notifications saved in `store`, bounded at [`INBOX_CAP`], and the
    /// ids the bound evicted, which the store should delete. A failed load logs and starts empty.
    #[must_use]
    pub fn load(store: Option<&Store>) -> (Inbox, Vec<String>) {
        let inbox = Inbox::default();
        let saved = match store.map(Store::load_notifications) {
            Some(Ok(saved)) => saved,
            Some(Err(error)) => {
                tracing::warn!("cannot load saved notifications: {error}");
                Vec::new()
            }
            None => Vec::new(),
        };
        let (kept, evicted) = bound_saved(saved);
        inbox.lock().items = kept;
        (inbox, evicted)
    }

    /// Every notification, newest first.
    #[must_use]
    pub fn list(&self) -> Vec<Notification> {
        let mut items = self.lock().items.clone();
        items.sort_by(|a, b| {
            b.created_ms
                .cmp(&a.created_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        items
    }

    /// Adds `notification`, or refreshes the known one with its id. A known pending one takes the
    /// new body and `updated_ms`; any other status stays as it is.
    pub fn upsert(&self, notification: Notification) -> Upserted {
        let mut state = self.lock();
        let Some(existing) = state.items.iter_mut().find(|n| n.id == notification.id) else {
            let evicted = insert_bounded(&mut state.items, notification);
            return Upserted {
                is_new: true,
                evicted,
            };
        };
        if existing.status == NotificationStatus::Pending {
            existing.body = notification.body;
            existing.updated_ms = notification.updated_ms;
        }
        Upserted::default()
    }

    /// Sets the status and result of notification `id` and returns it, or `None` for an unknown id.
    pub fn set_status(
        &self,
        id: &str,
        status: NotificationStatus,
        result: Option<String>,
    ) -> Option<Notification> {
        let mut state = self.lock();
        let notification = state.items.iter_mut().find(|n| n.id == id)?;
        notification.status = status;
        notification.result = result;
        notification.updated_ms = now_ms();
        Some(notification.clone())
    }

    /// The notification `id`, when the inbox holds it.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Notification> {
        self.lock().items.iter().find(|n| n.id == id).cloned()
    }

    /// How many pending notifications are not muted.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.lock()
            .items
            .iter()
            .filter(|n| n.status == NotificationStatus::Pending && !n.muted)
            .count()
    }

    /// Upserts a guard event unless one with the same source, profile, model and rule came within
    /// [`GUARD_DEDUPE_MS`] before `now_ms`. Returns `None` when it skipped the event.
    pub fn note_guard(&self, notification: Notification, now_ms: u64) -> Option<Upserted> {
        let key = (
            notification.source.clone(),
            notification.profile.clone(),
            notification.model.clone(),
            notification.rule.clone(),
        );
        {
            let mut state = self.lock();
            state
                .guards
                .retain(|_, seen| now_ms.saturating_sub(*seen) < GUARD_DEDUPE_MS);
            if state.guards.contains_key(&key) {
                return None;
            }
            state.guards.insert(key, now_ms);
        }
        Some(self.upsert(notification))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Splits `saved` into the [`INBOX_CAP`] notifications to keep and the evicted ids, by the rule of
/// [`insert_bounded`]: settled ones go first, oldest first, then pending ones, oldest first.
fn bound_saved(mut saved: Vec<Notification>) -> (Vec<Notification>, Vec<String>) {
    let excess = saved.len().saturating_sub(INBOX_CAP);
    if excess == 0 {
        return (saved, Vec::new());
    }
    saved.sort_by(|a, b| (is_open(a), a.created_ms, &a.id).cmp(&(is_open(b), b.created_ms, &b.id)));
    let kept = saved.split_off(excess);
    (kept, saved.into_iter().map(|n| n.id).collect())
}

/// Pushes `notification`, then evicts the oldest settled entry, or the oldest one when all are
/// pending, and returns the evicted id.
fn insert_bounded(items: &mut Vec<Notification>, notification: Notification) -> Option<String> {
    items.push(notification);
    if items.len() <= INBOX_CAP {
        return None;
    }
    let oldest = |pending: bool| {
        items
            .iter()
            .enumerate()
            .filter(|(_, n)| pending || !is_open(n))
            .min_by_key(|(_, n)| n.created_ms)
            .map(|(index, _)| index)
    };
    let index = oldest(false).or_else(|| oldest(true))?;
    Some(items.remove(index).id)
}

/// True while a notification still waits on a person or on an apply that is running.
fn is_open(notification: &Notification) -> bool {
    matches!(
        notification.status,
        NotificationStatus::Pending | NotificationStatus::Approved
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::notification::{NotificationKind, NotificationSeverity};

    fn notification(id: &str, created_ms: u64) -> Notification {
        Notification {
            id: id.into(),
            kind: NotificationKind::Guard,
            source: "local_tuner".into(),
            severity: NotificationSeverity::Warning,
            title: "Prefill guard".into(),
            body: "first".into(),
            profile: Some("local".into()),
            model: Some("gemma".into()),
            rule: Some("prefill_guard".into()),
            diff: None,
            actions: Notification::default_actions(NotificationKind::Guard),
            status: NotificationStatus::Pending,
            created_ms,
            updated_ms: created_ms,
            result: None,
            muted: false,
        }
    }

    fn memory_store() -> Store {
        Store::open("sqlite", Some(":memory:")).unwrap()
    }

    #[test]
    fn list_is_newest_first() {
        let inbox = Inbox::default();
        assert!(inbox.upsert(notification("a", 1)).is_new);
        assert!(inbox.upsert(notification("b", 3)).is_new);
        assert!(inbox.upsert(notification("c", 2)).is_new);
        let ids: Vec<_> = inbox.list().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, ["b", "c", "a"]);
    }

    #[test]
    fn upsert_refreshes_a_pending_one() {
        let inbox = Inbox::default();
        inbox.upsert(notification("a", 1));
        let mut again = notification("a", 9);
        again.body = "second".into();
        again.title = "ignored".into();
        assert!(!inbox.upsert(again).is_new);
        let stored = inbox.get("a").unwrap();
        assert_eq!(stored.body, "second");
        assert_eq!(stored.updated_ms, 9);
        assert_eq!(stored.created_ms, 1);
        assert_eq!(stored.title, "Prefill guard");
    }

    #[test]
    fn upsert_keeps_a_settled_status() {
        let inbox = Inbox::default();
        inbox.upsert(notification("a", 1));
        inbox.set_status("a", NotificationStatus::Dismissed, None);
        let mut again = notification("a", 9);
        again.body = "second".into();
        assert!(!inbox.upsert(again).is_new);
        let stored = inbox.get("a").unwrap();
        assert_eq!(stored.status, NotificationStatus::Dismissed);
        assert_eq!(stored.body, "first");
    }

    #[test]
    fn set_status_updates_or_reports_unknown() {
        let inbox = Inbox::default();
        inbox.upsert(notification("a", 1));
        let updated = inbox
            .set_status("a", NotificationStatus::Failed, Some("tail".into()))
            .unwrap();
        assert_eq!(updated.status, NotificationStatus::Failed);
        assert_eq!(updated.result.as_deref(), Some("tail"));
        assert!(updated.updated_ms > 1);
        assert!(
            inbox
                .set_status("x", NotificationStatus::Applied, None)
                .is_none()
        );
        assert!(inbox.get("x").is_none());
    }

    #[test]
    fn pending_count_skips_muted_and_settled() {
        let inbox = Inbox::default();
        inbox.upsert(notification("a", 1));
        let mut muted = notification("b", 2);
        muted.muted = true;
        inbox.upsert(muted);
        inbox.upsert(notification("c", 3));
        inbox.set_status("c", NotificationStatus::Applied, None);
        assert_eq!(inbox.pending_count(), 1);
    }

    #[test]
    fn bound_evicts_the_oldest_settled_first() {
        let inbox = Inbox::default();
        for index in 0..INBOX_CAP {
            inbox.upsert(notification(&format!("n{index}"), index as u64 + 10));
        }
        inbox.set_status("n5", NotificationStatus::Dismissed, None);
        inbox.set_status("n7", NotificationStatus::Applied, None);
        let upserted = inbox.upsert(notification("new", 10_000));
        assert_eq!(upserted.evicted.as_deref(), Some("n5"));
        assert_eq!(inbox.list().len(), INBOX_CAP);
        assert!(inbox.get("n5").is_none());
        assert!(inbox.get("n7").is_some());
        assert!(inbox.get("n0").is_some());
    }

    #[test]
    fn bound_keeps_an_approved_entry_while_its_apply_runs() {
        let inbox = Inbox::default();
        for index in 0..INBOX_CAP {
            inbox.upsert(notification(&format!("n{index}"), index as u64 + 10));
        }
        inbox.set_status("n0", NotificationStatus::Approved, None);
        inbox.set_status("n9", NotificationStatus::Dismissed, None);
        let upserted = inbox.upsert(notification("new", 10_000));
        assert_eq!(upserted.evicted.as_deref(), Some("n9"));
        assert!(inbox.get("n0").is_some());
    }

    #[test]
    fn bound_evicts_the_oldest_when_all_are_pending() {
        let inbox = Inbox::default();
        for index in 0..INBOX_CAP {
            assert_eq!(
                inbox
                    .upsert(notification(&format!("n{index}"), index as u64 + 10))
                    .evicted,
                None
            );
        }
        let upserted = inbox.upsert(notification(&format!("n{INBOX_CAP}"), 10_000));
        assert_eq!(upserted.evicted.as_deref(), Some("n0"));
        assert_eq!(inbox.list().len(), INBOX_CAP);
        assert!(inbox.get("n0").is_none());
        assert!(inbox.get(&format!("n{INBOX_CAP}")).is_some());
    }

    #[test]
    fn note_guard_dedupes_within_the_window() {
        let inbox = Inbox::default();
        assert!(inbox.note_guard(notification("g1", 0), 1_000).is_some());
        assert!(
            inbox
                .note_guard(notification("g2", 0), 1_000 + GUARD_DEDUPE_MS - 1)
                .is_none()
        );
        assert!(inbox.get("g2").is_none());
        let mut other_model = notification("g3", 0);
        other_model.model = Some("qwen".into());
        assert!(inbox.note_guard(other_model, 2_000).is_some());
        assert!(
            inbox
                .note_guard(notification("g4", 0), 1_000 + GUARD_DEDUPE_MS)
                .is_some()
        );
        assert!(inbox.get("g4").is_some());
    }

    #[test]
    fn note_guard_keeps_a_window_per_profile() {
        let inbox = Inbox::default();
        assert!(inbox.note_guard(notification("g1", 0), 1_000).is_some());
        let mut other_profile = notification("g2", 0);
        other_profile.profile = Some("remote".into());
        assert!(inbox.note_guard(other_profile.clone(), 1_001).is_some());
        assert!(inbox.get("g2").is_some());
        other_profile.id = "g3".into();
        assert!(inbox.note_guard(other_profile, 1_002).is_none());
    }

    #[test]
    fn load_reads_saved_notifications() {
        let store = memory_store();
        store.save_notification(&notification("a", 1)).unwrap();
        store.save_notification(&notification("b", 2)).unwrap();
        let (inbox, evicted) = Inbox::load(Some(&store));
        let ids: Vec<_> = inbox.list().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, ["b", "a"]);
        assert!(evicted.is_empty());
        let (empty, evicted) = Inbox::load(None);
        assert!(empty.list().is_empty());
        assert!(evicted.is_empty());
    }

    #[test]
    fn load_bounds_the_saved_notifications_and_reports_the_evicted() {
        let store = memory_store();
        let mut settled = notification("settled", 5);
        settled.status = NotificationStatus::Dismissed;
        store.save_notification(&settled).unwrap();
        for index in 0..INBOX_CAP {
            store
                .save_notification(&notification(&format!("n{index}"), index as u64 + 10))
                .unwrap();
        }
        store.save_notification(&notification("oldest", 1)).unwrap();
        let (inbox, evicted) = Inbox::load(Some(&store));
        assert_eq!(evicted, ["settled", "oldest"]);
        assert_eq!(inbox.list().len(), INBOX_CAP);
        assert!(inbox.get("n0").is_some());
    }
}

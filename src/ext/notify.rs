use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::schemas::notification::{
    Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};
use crate::services::notifications::dispatch::Notifier;
use crate::services::rate_limiter::{self, RateLimiter, Take};
use crate::services::store::now_ms;
use crate::utils::hex::to_hex;

/// How urgent an extension's notification is. The order runs from `Info` up to `Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum ExtSeverity {
    /// Shown in the inbox. The sinks skip it unless `LLM_HUB_NOTIFY_MIN_SEVERITY` is `info`.
    Info,
    /// Shown in the inbox and sent to the sinks with the default minimum severity.
    Warning,
    /// Shown in the inbox and sent to every sink.
    Critical,
}

impl ExtSeverity {
    fn schema(self) -> NotificationSeverity {
        match self {
            ExtSeverity::Info => NotificationSeverity::Info,
            ExtSeverity::Warning => NotificationSeverity::Warning,
            ExtSeverity::Critical => NotificationSeverity::Critical,
        }
    }
}

/// A notification an extension sends with `ctx.notify(..)` in any phase. The hub stores it in
/// the Notifications inbox with source `ext:<extension name>` and sends it to the sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExtNotification {
    /// How urgent it is.
    pub severity: ExtSeverity,
    /// A one-line summary.
    pub title: String,
    /// The main text.
    pub body: String,
    /// The model it is about, such as `local/gemma`, when it is about one.
    pub model: Option<String>,
    /// The profile it is about, when it is about one.
    pub profile: Option<String>,
}

impl ExtNotification {
    /// A notification with no model and no profile.
    #[must_use]
    pub fn new(
        severity: ExtSeverity,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> ExtNotification {
        ExtNotification {
            severity,
            title: title.into(),
            body: body.into(),
            model: None,
            profile: None,
        }
    }

    /// The notification about `model`.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> ExtNotification {
        self.model = Some(model.into());
        self
    }

    /// The notification about `profile`.
    #[must_use]
    pub fn with_profile(mut self, profile: impl Into<String>) -> ExtNotification {
        self.profile = Some(profile.into());
        self
    }

    /// The inbox notification from the extension named `ext`.
    fn into_notification(self, ext: &str, now_ms: u64) -> Notification {
        let kind = NotificationKind::Custom;
        Notification {
            id: format!("ext-{}", to_hex(&rand::random::<[u8; 8]>())),
            kind,
            source: format!("ext:{ext}"),
            severity: self.severity.schema(),
            title: self.title,
            body: self.body,
            profile: self.profile,
            model: self.model,
            rule: None,
            diff: None,
            actions: Notification::default_actions(kind),
            status: NotificationStatus::Pending,
            created_ms: now_ms,
            updated_ms: now_ms,
            result: None,
            muted: false,
        }
    }
}

/// The notifications one extension sent during one phase call. The extension set drains it after
/// the call, so it knows which extension sent them.
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    pending: AtomicBool,
    notes: Mutex<Vec<ExtNotification>>,
}

impl Outbox {
    pub(crate) fn push(&self, note: ExtNotification) {
        self.notes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(note);
        self.pending.store(true, Ordering::Release);
    }

    /// Empties the outbox. Without a notification it skips the lock.
    pub(crate) fn take(&self) -> Vec<ExtNotification> {
        if !self.pending.swap(false, Ordering::Acquire) {
            return Vec::new();
        }
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Drains `outbox` into `notifier` as notifications from the extension named `ext`. Without a
/// notifier, or past the shared limit of 60 a minute for source `ext:<name>`, they drop with a
/// debug log.
pub(crate) fn deliver(notifier: Option<&Notifier>, ext: &str, outbox: &Outbox) {
    deliver_limited(
        notifier,
        &rate_limiter::NOTIFICATIONS,
        ext,
        outbox,
        now_ms(),
    );
}

fn deliver_limited(
    notifier: Option<&Notifier>,
    limiter: &RateLimiter,
    ext: &str,
    outbox: &Outbox,
    now: u64,
) {
    let notes = outbox.take();
    if notes.is_empty() {
        return;
    }
    let Some(notifier) = notifier else {
        tracing::debug!(
            extension = ext,
            count = notes.len(),
            "no notifier; extension notifications dropped"
        );
        return;
    };
    let source = format!("ext:{ext}");
    for note in notes {
        match limiter.take(&source, now) {
            Take::Allowed => {
                notifier.notify(note.into_notification(ext, now));
            }
            Take::Limited { first_in_window } => {
                if first_in_window {
                    tracing::debug!(
                        extension = ext,
                        limit = rate_limiter::NOTIFICATIONS_PER_MINUTE,
                        "extension notifications over the per-minute limit; dropping"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configs::NotifyConfig;

    #[test]
    fn builder_sets_model_and_profile() {
        let note = ExtNotification::new(ExtSeverity::Info, "t", "b")
            .with_model("local/gemma")
            .with_profile("local");

        assert_eq!(note.severity, ExtSeverity::Info);
        assert_eq!(note.title, "t");
        assert_eq!(note.body, "b");
        assert_eq!(note.model.as_deref(), Some("local/gemma"));
        assert_eq!(note.profile.as_deref(), Some("local"));
    }

    #[test]
    fn severities_map_to_the_inbox_severities() {
        for (ext, inbox) in [
            (ExtSeverity::Info, NotificationSeverity::Info),
            (ExtSeverity::Warning, NotificationSeverity::Warning),
            (ExtSeverity::Critical, NotificationSeverity::Critical),
        ] {
            assert_eq!(ext.schema(), inbox);
        }
    }

    #[test]
    fn a_notification_becomes_a_pending_custom_inbox_entry() {
        let notification = ExtNotification::new(ExtSeverity::Critical, "Disk", "full")
            .with_model("m")
            .into_notification("audit", 7);

        assert!(notification.id.starts_with("ext-"));
        assert_eq!(notification.source, "ext:audit");
        assert_eq!(notification.kind, NotificationKind::Custom);
        assert_eq!(notification.severity, NotificationSeverity::Critical);
        assert_eq!(notification.status, NotificationStatus::Pending);
        assert_eq!(
            notification.actions,
            Notification::default_actions(NotificationKind::Custom)
        );
        assert_eq!(notification.model.as_deref(), Some("m"));
        assert_eq!((notification.created_ms, notification.updated_ms), (7, 7));
    }

    #[test]
    fn take_empties_the_outbox() {
        let outbox = Outbox::default();
        assert!(outbox.take().is_empty());

        outbox.push(ExtNotification::new(ExtSeverity::Info, "a", ""));
        outbox.push(ExtNotification::new(ExtSeverity::Info, "b", ""));

        assert_eq!(outbox.take().len(), 2);
        assert!(outbox.take().is_empty());
    }

    #[test]
    fn deliver_stores_each_notification_and_empties_the_outbox() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let outbox = Outbox::default();
        outbox.push(ExtNotification::new(ExtSeverity::Warning, "a", "b"));

        deliver(Some(&notifier), "probe", &outbox);

        let listed = notifier.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].source, "ext:probe");
        assert!(outbox.take().is_empty());
    }

    #[test]
    fn deliver_drops_what_passes_the_per_minute_limit() {
        let notifier = Notifier::new(&NotifyConfig::default(), None);
        let limiter = RateLimiter::new(2, 60_000);
        let outbox = Outbox::default();
        for title in ["a", "b", "c"] {
            outbox.push(ExtNotification::new(ExtSeverity::Warning, title, ""));
        }

        deliver_limited(Some(&notifier), &limiter, "flood", &outbox, 0);
        assert_eq!(notifier.list().len(), 2);
        outbox.push(ExtNotification::new(ExtSeverity::Warning, "d", ""));
        deliver_limited(Some(&notifier), &limiter, "other", &outbox, 0);
        assert_eq!(
            notifier.list().len(),
            3,
            "each extension has its own bucket"
        );
        outbox.push(ExtNotification::new(ExtSeverity::Warning, "e", ""));
        deliver_limited(Some(&notifier), &limiter, "flood", &outbox, 30_000);
        assert_eq!(
            notifier.list().len(),
            4,
            "the bucket refills over the minute"
        );
        assert_ne!(limiter.take("ext:flood", 30_000), Take::Allowed);
    }

    #[test]
    fn deliver_without_a_notifier_drops_the_notifications() {
        let outbox = Outbox::default();
        outbox.push(ExtNotification::new(ExtSeverity::Warning, "a", "b"));

        deliver(None, "probe", &outbox);

        assert!(outbox.take().is_empty());
    }
}

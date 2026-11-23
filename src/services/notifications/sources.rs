//! The built-in notification sources: a new error group and a profile health flip.
//!
//! Both read and write memory only, so a request that fails pays one short lock.

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use axum::http::StatusCode;

use crate::schemas::notification::{
    Notification, NotificationKind, NotificationSeverity, NotificationStatus,
};
use crate::services::error_signature::error_signature;
use crate::services::harness::is_keep_warm;
use crate::services::stats::RequestOutcome;
use crate::services::store::hash_key;

/// The source of a new error group notification.
pub const ERRORS_SOURCE: &str = "errors";
/// The source of a profile health notification.
pub const HEALTH_SOURCE: &str = "health";
/// The most error signatures the watch remembers. Past it, new groups no longer notify.
pub const SEEN_CAP: usize = 4096;
/// The most new groups held while the watch waits for the stored groups.
pub const WAITING_CAP: usize = 64;
/// Longest first message line a new error body carries, in characters.
const BODY_LINE_CHARS: usize = 400;
/// Longest signature a new error title carries, in characters.
const TITLE_SIGNATURE_CHARS: usize = 80;
/// Hex characters of the signature hash in a new error id.
const ID_HASH_CHARS: usize = 16;

#[derive(Debug, Default)]
struct WatchState {
    /// True once the groups already in the store are in `seen`.
    seeded: bool,
    seen: HashSet<String>,
    /// New groups found before the seed, with their notification.
    waiting: Vec<(String, Notification)>,
}

/// The error signatures the hub has seen, so only the first failure of a group notifies.
#[derive(Debug, Default)]
pub struct ErrorWatch {
    state: Mutex<WatchState>,
}

impl ErrorWatch {
    /// The notification for `outcome` when it starts a new error group, else `None`.
    /// Before [`Self::seed`] runs, the watch holds a new group and `seed` returns it.
    pub fn observe(&self, outcome: &RequestOutcome, now_ms: u64) -> Option<Notification> {
        if !counts_as_error(outcome) {
            return None;
        }
        let signature = error_signature(outcome.error.as_deref(), outcome.status);
        let mut state = self.lock();
        if state.seen.contains(&signature) || state.seen.len() >= SEEN_CAP {
            return None;
        }
        if !state.seeded {
            let known = state.waiting.iter().any(|(held, _)| *held == signature);
            if !known && state.waiting.len() < WAITING_CAP {
                let notification = error_notification(&signature, outcome, now_ms);
                state.waiting.push((signature, notification));
            }
            return None;
        }
        state.seen.insert(signature.clone());
        drop(state);
        Some(error_notification(&signature, outcome, now_ms))
    }

    /// Marks the groups of the stored failures `signatures` as seen and returns the held new
    /// groups that are not among them.
    pub fn seed(&self, signatures: impl IntoIterator<Item = String>) -> Vec<Notification> {
        let mut state = self.lock();
        for signature in signatures {
            if state.seen.len() >= SEEN_CAP {
                break;
            }
            state.seen.insert(signature);
        }
        state.seeded = true;
        let waiting = std::mem::take(&mut state.waiting);
        let mut fresh = Vec::new();
        for (signature, notification) in waiting {
            if state.seen.len() < SEEN_CAP && state.seen.insert(signature) {
                fresh.push(notification);
            }
        }
        fresh
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A failure the Errors page lists: a 4xx or 5xx status that is not a keep-warm ping.
fn counts_as_error(outcome: &RequestOutcome) -> bool {
    StatusCode::from_u16(outcome.status)
        .is_ok_and(|status| status.is_client_error() || status.is_server_error())
        && !is_keep_warm(outcome.client.as_deref())
}

/// The notification for the first failure of the group `signature`. Its id hashes the
/// signature, so the inbox keeps one per group.
#[must_use]
pub fn error_notification(signature: &str, outcome: &RequestOutcome, now_ms: u64) -> Notification {
    let hash = hash_key(signature);
    let model = bare_model(&outcome.profile, &outcome.model_key);
    let line = outcome
        .error
        .as_deref()
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map_or_else(
            || "(no reason recorded)".to_owned(),
            |line| truncate(line, BODY_LINE_CHARS),
        );
    let body = format!(
        "Profile: {}\nModel: {model}\nStatus: {}\n\n{line}",
        outcome.profile, outcome.status
    );
    Notification {
        id: format!("{ERRORS_SOURCE}:{}", &hash[..ID_HASH_CHARS.min(hash.len())]),
        kind: NotificationKind::Advisory,
        source: ERRORS_SOURCE.to_owned(),
        severity: NotificationSeverity::Warning,
        title: format!("New error: {}", truncate(signature, TITLE_SIGNATURE_CHARS)),
        body,
        profile: Some(outcome.profile.clone()),
        model: Some(model.to_owned()),
        rule: None,
        diff: None,
        actions: Notification::default_actions(NotificationKind::Advisory),
        status: NotificationStatus::Pending,
        created_ms: now_ms,
        updated_ms: now_ms,
        result: None,
        muted: false,
    }
}

/// The profiles last reported unhealthy, so only a flip notifies.
#[derive(Debug, Default)]
pub struct HealthWatch {
    unhealthy: Mutex<HashSet<String>>,
}

impl HealthWatch {
    /// The notification for a health flip of `profile`: critical when it turns unhealthy, info
    /// when it turns healthy after it was unhealthy. `None` when the flag did not flip.
    pub fn observe(
        &self,
        profile: &str,
        healthy: bool,
        reason: &str,
        now_ms: u64,
    ) -> Option<Notification> {
        let mut unhealthy = self
            .unhealthy
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let flipped = if healthy {
            unhealthy.remove(profile)
        } else {
            unhealthy.insert(profile.to_owned())
        };
        drop(unhealthy);
        flipped.then(|| health_notification(profile, healthy, reason, now_ms))
    }
}

/// The notification for `profile` turning unhealthy, or healthy again, at `now_ms`.
#[must_use]
pub fn health_notification(
    profile: &str,
    healthy: bool,
    reason: &str,
    now_ms: u64,
) -> Notification {
    let (state, severity, title) = if healthy {
        (
            "recovered",
            NotificationSeverity::Info,
            format!("Profile {profile} recovered"),
        )
    } else {
        (
            "unhealthy",
            NotificationSeverity::Critical,
            format!("Profile {profile} is unhealthy"),
        )
    };
    Notification {
        id: format!("{HEALTH_SOURCE}:{profile}:{state}:{now_ms}"),
        kind: NotificationKind::Advisory,
        source: HEALTH_SOURCE.to_owned(),
        severity,
        title,
        body: truncate(reason, BODY_LINE_CHARS),
        profile: Some(profile.to_owned()),
        model: None,
        rule: None,
        diff: None,
        actions: Notification::default_actions(NotificationKind::Advisory),
        status: NotificationStatus::Pending,
        created_ms: now_ms,
        updated_ms: now_ms,
        result: None,
        muted: false,
    }
}

/// `model_key` without its `profile/` prefix.
fn bare_model<'a>(profile: &str, model_key: &'a str) -> &'a str {
    model_key
        .strip_prefix(profile)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model_key)
}

/// `text` cut to `max` characters, with an ellipsis when cut.
fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", text[..cut].trim_end()),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::KEEP_WARM_CLIENT;

    fn failure(status: u16, error: &str) -> RequestOutcome {
        RequestOutcome {
            profile: "local".into(),
            model_key: "local/gemma".into(),
            status,
            error: Some(error.into()),
            ..RequestOutcome::default()
        }
    }

    fn seeded() -> ErrorWatch {
        let watch = ErrorWatch::default();
        assert!(watch.seed(Vec::new()).is_empty());
        watch
    }

    #[test]
    fn first_failure_of_a_group_notifies_once() {
        let watch = seeded();
        let first = watch
            .observe(&failure(500, "boom at 2026-11-21T10:00:00Z\nstack"), 7)
            .unwrap();
        assert_eq!(first.source, "errors");
        assert_eq!(first.kind, NotificationKind::Advisory);
        assert_eq!(first.severity, NotificationSeverity::Warning);
        assert_eq!(first.title, "New error: boom at <ts>");
        assert_eq!(
            first.body,
            "Profile: local\nModel: gemma\nStatus: 500\n\nboom at 2026-11-21T10:00:00Z"
        );
        assert_eq!(first.profile.as_deref(), Some("local"));
        assert_eq!(first.model.as_deref(), Some("gemma"));
        assert_eq!(first.created_ms, 7);
        assert!(first.id.starts_with("errors:"));
        assert_eq!(first.id.len(), "errors:".len() + ID_HASH_CHARS);
        // The same group with another timestamp is a repeat.
        assert!(
            watch
                .observe(&failure(500, "boom at 2026-11-21T11:00:00Z"), 8)
                .is_none()
        );
        let other = watch.observe(&failure(500, "other failure"), 9).unwrap();
        assert_ne!(other.id, first.id);
    }

    #[test]
    fn the_id_is_stable_for_a_signature() {
        let a = error_notification("sig", &failure(500, "x"), 1);
        let b = error_notification("sig", &failure(502, "y"), 2);
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn successes_and_keep_warm_pings_never_notify() {
        let watch = seeded();
        assert!(watch.observe(&failure(200, "fine"), 1).is_none());
        assert!(watch.observe(&failure(399, "redirect"), 1).is_none());
        let mut ping = failure(500, "down");
        ping.client = Some(KEEP_WARM_CLIENT.into());
        assert!(watch.observe(&ping, 1).is_none());
        assert!(watch.observe(&failure(500, "down"), 1).is_some());
    }

    #[test]
    fn a_missing_reason_falls_back_to_the_status() {
        let mut outcome = failure(503, "");
        outcome.error = None;
        let notification = seeded().observe(&outcome, 1).unwrap();
        assert_eq!(
            notification.title,
            "New error: HTTP 503 (no reason recorded)"
        );
        assert!(notification.body.ends_with("(no reason recorded)"));
    }

    #[test]
    fn long_lines_are_cut() {
        let long = "x".repeat(1000);
        let notification = seeded().observe(&failure(500, &long), 1).unwrap();
        let line = notification.body.lines().last().unwrap();
        assert_eq!(line.chars().count(), BODY_LINE_CHARS + 1);
        assert!(line.ends_with('…'));
        assert!(notification.title.ends_with('…'));
    }

    #[test]
    fn stored_groups_never_notify_and_held_new_groups_do_after_the_seed() {
        let watch = ErrorWatch::default();
        assert!(watch.observe(&failure(500, "old failure"), 1).is_none());
        assert!(watch.observe(&failure(500, "new failure"), 2).is_none());
        assert!(watch.observe(&failure(500, "new failure"), 3).is_none());
        let fresh = watch.seed(vec![error_signature(Some("old failure"), 500)]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].title, "New error: new failure");
        assert_eq!(fresh[0].created_ms, 2);
        assert!(watch.observe(&failure(500, "old failure"), 4).is_none());
        assert!(watch.observe(&failure(500, "new failure"), 5).is_none());
    }

    #[test]
    fn the_watch_stops_at_its_caps() {
        let watch = ErrorWatch::default();
        for n in 0..=WAITING_CAP {
            assert!(
                watch
                    .observe(&failure(500, &format!("held {n}x")), 1)
                    .is_none()
            );
        }
        let full: Vec<String> = (0..SEEN_CAP).map(|n| format!("seen {n}")).collect();
        assert!(watch.seed(full).is_empty());
        assert!(watch.observe(&failure(500, "one more"), 1).is_none());
    }

    #[test]
    fn health_notifies_once_per_flip() {
        let watch = HealthWatch::default();
        assert!(watch.observe("local", true, "up", 1).is_none());
        let down = watch
            .observe("local", false, "connection refused", 2)
            .unwrap();
        assert_eq!(down.id, "health:local:unhealthy:2");
        assert_eq!(down.source, "health");
        assert_eq!(down.severity, NotificationSeverity::Critical);
        assert_eq!(down.title, "Profile local is unhealthy");
        assert_eq!(down.body, "connection refused");
        assert_eq!(down.profile.as_deref(), Some("local"));
        assert!(watch.observe("local", false, "still refused", 3).is_none());
        let up = watch.observe("local", true, "models listed", 4).unwrap();
        assert_eq!(up.id, "health:local:recovered:4");
        assert_eq!(up.severity, NotificationSeverity::Info);
        assert_eq!(up.title, "Profile local recovered");
        assert!(watch.observe("local", true, "models listed", 5).is_none());
        assert!(watch.observe("other", true, "up", 6).is_none());
    }

    #[test]
    fn bare_model_keeps_a_key_without_the_profile_prefix() {
        assert_eq!(bare_model("local", "local/gemma"), "gemma");
        assert_eq!(bare_model("local", "localx/gemma"), "localx/gemma");
        assert_eq!(bare_model("local", "gemma"), "gemma");
    }
}

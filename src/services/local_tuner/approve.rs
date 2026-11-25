//! Approval flow: registry edit, apply command, then status update.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::dependencies::state::AppState;
use crate::schemas::notification::{FlagDiff, Notification, NotificationKind, NotificationStatus};
use crate::services::local_tuner::{command, registry};
use crate::services::store::now_ms;

/// Why an approve or dismiss request did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveError {
    /// No notification has this id.
    NotFound,
    /// The notification is no longer pending.
    NotPending,
    /// Only a proposal can be approved.
    NotProposal,
    /// The proposal names no profile, or a profile the config no longer has.
    UnknownProfile,
    /// The profile sets no `TUNER_REGISTRY`, so there is no file to edit.
    NoRegistry,
    /// The proposal carries no flag change.
    NoDiff,
    /// The proposal names no model.
    NoModel,
    /// Another approval for the same profile is running.
    Busy,
}

impl fmt::Display for ApproveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFound => "notification not found",
            Self::NotPending => "this notification is no longer pending",
            Self::NotProposal => "only a proposal can be approved",
            Self::UnknownProfile => "the proposal's profile is not configured",
            Self::NoRegistry => {
                "the profile has no model registry; set LLM_HUB_<PROFILE>_TUNER_REGISTRY"
            }
            Self::NoDiff => "the proposal has no flag change to apply",
            Self::NoModel => "the proposal names no model",
            Self::Busy => "another approval for this profile is running; try again shortly",
        })
    }
}

impl std::error::Error for ApproveError {}

/// Applies the proposal `id`: it edits the profile's registry, runs the apply command when one is
/// configured, and records `applied` or `failed`. An apply failure is returned as `Ok` with status
/// `failed`, because the notification records it.
///
/// # Errors
/// An [`ApproveError`] when the notification cannot be approved; its status then stays unchanged.
pub async fn approve(state: &AppState, id: &str) -> Result<Notification, ApproveError> {
    let notifier = state.notifier();
    let notification = notifier.get(id).ok_or(ApproveError::NotFound)?;
    if notification.kind != NotificationKind::Proposal {
        return Err(ApproveError::NotProposal);
    }
    if notification.status != NotificationStatus::Pending {
        return Err(ApproveError::NotPending);
    }
    let profile = notification
        .profile
        .clone()
        .ok_or(ApproveError::UnknownProfile)?;
    let tuner = state
        .config()
        .profile(&profile)
        .ok_or(ApproveError::UnknownProfile)?
        .tuner
        .clone();
    let path = tuner.registry.ok_or(ApproveError::NoRegistry)?;
    let diff = notification.diff.clone().ok_or(ApproveError::NoDiff)?;
    let model = notification.model.clone().ok_or(ApproveError::NoModel)?;
    let guard = state
        .local_tuner()
        .apply_lock(&profile)
        .try_lock_owned()
        .map_err(|_| ApproveError::Busy)?;
    // Another approval may have finished between the first read and the lock.
    if notifier.get(id).map(|n| n.status) != Some(NotificationStatus::Pending) {
        return Err(ApproveError::NotPending);
    }
    notifier
        .set_status(id, NotificationStatus::Approved, None)
        .ok_or(ApproveError::NotFound)?;

    // The task owns the lock and the apply, so a dropped request cannot stop them midway.
    let task = tokio::spawn({
        let notifier = Arc::clone(notifier);
        let state = state.clone();
        let id = id.to_owned();
        let profile_name = profile.clone();
        async move {
            let _guard = guard;
            let (status, result) = apply(
                path,
                model.clone(),
                diff,
                tuner.apply_command,
                tuner.apply_timeout,
            )
            .await;
            if status == NotificationStatus::Applied
                && state
                    .config()
                    .profile(&profile_name)
                    .is_some_and(crate::configs::ProfileConfig::is_local)
                && state.local_pool().rows(&profile_name).is_some()
            {
                // The registry edit is on disk; reload so the restart reads the new flags.
                state.local_pool().reload_now();
                let restart = state
                    .local_pool()
                    .restart_when_idle(&profile_name, &model, Duration::from_secs(30))
                    .await;
                match restart {
                    Ok(()) => {
                        let _ = super::probe::run_once(&state, now_ms()).await;
                        notifier.set_status(&id, status, Some(result))
                    }
                    Err(error) => notifier.set_status(
                        &id,
                        NotificationStatus::Failed,
                        Some(format!("registry updated; {error}")),
                    ),
                }
            } else {
                notifier.set_status(&id, status, Some(result))
            }
        }
    });
    let done = match task.await {
        Ok(done) => done,
        Err(error) => notifier.set_status(
            id,
            NotificationStatus::Failed,
            Some(format!("apply did not finish: {error}")),
        ),
    };
    done.ok_or(ApproveError::NotFound)
}

/// Edits the registry, then runs the apply command when one is configured, and returns the final
/// status with its result text.
async fn apply(
    path: PathBuf,
    model: String,
    diff: FlagDiff,
    apply_command: Option<Vec<String>>,
    timeout: Duration,
) -> (NotificationStatus, String) {
    match edit_registry(path, model, diff).await {
        Err(error) => (NotificationStatus::Failed, error),
        Ok(summary) => match apply_command {
            None => (NotificationStatus::Applied, summary),
            Some(argv) => {
                let run = command::run(&argv, None, timeout).await;
                if run.ok {
                    (
                        NotificationStatus::Applied,
                        format!("{summary}; command: ok\n{}", run.tail),
                    )
                } else {
                    (
                        NotificationStatus::Failed,
                        format!("{summary}; command failed\n{}", run.tail),
                    )
                }
            }
        },
    }
}

/// Dismisses the pending notification `id`.
///
/// # Errors
/// [`ApproveError::NotFound`] for an unknown id, [`ApproveError::NotPending`] when it is not pending.
#[allow(
    clippy::unused_async,
    reason = "Async like `approve`, so the route awaits both the same way"
)]
pub async fn dismiss(state: &AppState, id: &str) -> Result<Notification, ApproveError> {
    let notifier = state.notifier();
    let notification = notifier.get(id).ok_or(ApproveError::NotFound)?;
    if notification.status != NotificationStatus::Pending {
        return Err(ApproveError::NotPending);
    }
    notifier
        .set_status(id, NotificationStatus::Dismissed, None)
        .ok_or(ApproveError::NotFound)
}

/// Runs the registry edit off the async runtime and returns a one-line summary or the error text.
async fn edit_registry(path: PathBuf, model: String, diff: FlagDiff) -> Result<String, String> {
    let now_s = now_ms() / 1_000;
    let outcome = tokio::task::spawn_blocking(move || registry::apply(&path, &model, &diff, now_s))
        .await
        .map_err(|e| format!("registry edit did not finish: {e}"))?
        .map_err(|e| e.to_string())?;
    Ok(match outcome.backup {
        Some(backup) => format!("registry updated (backup {})", backup.display()),
        None => "registry already up to date".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::notification::NotificationSeverity;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    /// A change to a pending proposal and the error approve then returns.
    type Case = (fn(&mut Notification), ApproveError);

    const ROW: &str = "gemma|org/repo|model.gguf|5|8192|Q4|--jinja\n";

    fn state(extra: &[(&str, &str)]) -> AppState {
        let mut vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "local"),
            ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_NOTIFICATIONS", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        vars.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn notification(kind: NotificationKind) -> Notification {
        Notification {
            id: "p1".into(),
            kind,
            source: "local_tuner".into(),
            severity: NotificationSeverity::Warning,
            title: "t".into(),
            body: "b".into(),
            profile: Some("local".into()),
            model: Some("gemma".into()),
            rule: Some("r".into()),
            diff: Some(FlagDiff {
                add: vec!["--cache-type-k".into(), "q8_0".into()],
                remove: Vec::new(),
                ctx: Some(16_384),
                old_ctx: Some(8_192),
            }),
            actions: Notification::default_actions(kind),
            status: NotificationStatus::Pending,
            created_ms: 1,
            updated_ms: 1,
            result: None,
            muted: false,
        }
    }

    /// A temp dir holding a registry with one `gemma` row, and its path as a string.
    fn registry_file(content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.conf");
        std::fs::write(&path, content).unwrap();
        let path = path.to_str().unwrap().to_owned();
        (dir, path)
    }

    fn notify(state: &AppState, notification: Notification) {
        assert!(state.notifier().notify(notification).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approve_edits_the_registry_runs_the_command_and_records_applied() {
        let (dir, path) = registry_file(ROW);
        let state = state(&[
            ("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str()),
            ("LLM_HUB_LOCAL_TUNER_APPLY_COMMAND", "sh -c 'echo synced'"),
        ]);
        notify(&state, notification(NotificationKind::Proposal));

        let done = approve(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Applied);
        let result = done.result.unwrap();
        assert!(result.starts_with("registry updated (backup "), "{result}");
        assert!(result.contains("; command: ok\nsynced"), "{result}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "gemma|org/repo|model.gguf|5|16384|Q4|--jinja --cache-type-k q8_0\n"
        );
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".bak-")
            })
            .count();
        assert_eq!(backups, 1);
        assert_eq!(
            state.notifier().get("p1").unwrap().status,
            NotificationStatus::Applied
        );
        assert_eq!(approve(&state, "p1").await, Err(ApproveError::NotPending));
    }

    #[tokio::test]
    async fn approve_without_a_command_records_applied_with_the_registry_summary() {
        let (_dir, path) = registry_file(ROW);
        let state = state(&[("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str())]);
        notify(&state, notification(NotificationKind::Proposal));

        let done = approve(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Applied);
        let result = done.result.unwrap();
        assert!(result.starts_with("registry updated (backup "), "{result}");
        assert!(!result.contains("command"), "{result}");
    }

    #[tokio::test]
    async fn approve_of_an_unchanged_row_says_already_up_to_date() {
        let (_dir, path) =
            registry_file("gemma|org/repo|model.gguf|5|16384|Q4|--jinja --cache-type-k q8_0\n");
        let state = state(&[("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str())]);
        notify(&state, notification(NotificationKind::Proposal));

        let done = approve(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Applied);
        assert_eq!(done.result.as_deref(), Some("registry already up to date"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failing_command_records_failed_with_its_output() {
        let (_dir, path) = registry_file(ROW);
        let state = state(&[
            ("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str()),
            (
                "LLM_HUB_LOCAL_TUNER_APPLY_COMMAND",
                "sh -c 'echo boom >&2; exit 3'",
            ),
        ]);
        notify(&state, notification(NotificationKind::Proposal));

        let done = approve(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Failed);
        let result = done.result.unwrap();
        assert!(result.contains("; command failed\n"), "{result}");
        assert!(result.contains("boom"), "{result}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_registry_row_records_failed_and_skips_the_command() {
        let (dir, path) = registry_file("other|org/repo|m.gguf|5|8192|Q4|--jinja\n");
        let marker = dir.path().join("ran");
        let command = format!("touch {}", marker.display());
        let state = state(&[
            ("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str()),
            ("LLM_HUB_LOCAL_TUNER_APPLY_COMMAND", command.as_str()),
        ]);
        notify(&state, notification(NotificationKind::Proposal));

        let done = approve(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Failed);
        assert_eq!(
            done.result.as_deref(),
            Some("model gemma not found in the registry")
        );
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_approve_still_finishes_the_apply() {
        let (dir, path) = registry_file(ROW);
        let marker = dir.path().join("done");
        let command = format!("sh -c 'sleep 0.3; touch {}'", marker.display());
        let state = state(&[
            ("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str()),
            ("LLM_HUB_LOCAL_TUNER_APPLY_COMMAND", command.as_str()),
        ]);
        notify(&state, notification(NotificationKind::Proposal));

        let early = tokio::time::timeout(Duration::from_millis(100), approve(&state, "p1")).await;

        assert!(early.is_err(), "approve should still run the command");
        assert_eq!(
            state.notifier().get("p1").unwrap().status,
            NotificationStatus::Approved
        );
        let mut status = NotificationStatus::Approved;
        for _ in 0..100 {
            status = state.notifier().get("p1").unwrap().status;
            if status != NotificationStatus::Approved {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(status, NotificationStatus::Applied);
        assert!(marker.exists(), "the command ran to the end");
        assert_eq!(approve(&state, "p1").await, Err(ApproveError::NotPending));
    }

    #[tokio::test]
    async fn approve_is_busy_while_the_profile_lock_is_held() {
        let (_dir, path) = registry_file(ROW);
        let state = state(&[("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str())]);
        notify(&state, notification(NotificationKind::Proposal));
        let lock = state.local_tuner().apply_lock("local");
        let held = lock.lock().await;

        assert_eq!(approve(&state, "p1").await, Err(ApproveError::Busy));
        assert_eq!(
            state.notifier().get("p1").unwrap().status,
            NotificationStatus::Pending
        );
        drop(held);
        assert_eq!(
            approve(&state, "p1").await.unwrap().status,
            NotificationStatus::Applied
        );
    }

    #[tokio::test]
    async fn approve_rejects_what_it_cannot_apply() {
        let (_dir, path) = registry_file(ROW);
        let with_registry = state(&[("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str())]);
        assert_eq!(
            approve(&with_registry, "missing").await,
            Err(ApproveError::NotFound)
        );

        notify(&with_registry, notification(NotificationKind::Advisory));
        assert_eq!(
            approve(&with_registry, "p1").await,
            Err(ApproveError::NotProposal)
        );

        let cases: [Case; 5] = [
            (
                |n| n.status = NotificationStatus::Dismissed,
                ApproveError::NotPending,
            ),
            (|n| n.profile = None, ApproveError::UnknownProfile),
            (
                |n| n.profile = Some("gone".into()),
                ApproveError::UnknownProfile,
            ),
            (|n| n.diff = None, ApproveError::NoDiff),
            (|n| n.model = None, ApproveError::NoModel),
        ];
        for (index, (change, expected)) in cases.into_iter().enumerate() {
            let state = state(&[("LLM_HUB_LOCAL_TUNER_REGISTRY", path.as_str())]);
            let mut proposal = notification(NotificationKind::Proposal);
            proposal.id = format!("case{index}");
            change(&mut proposal);
            notify(&state, proposal.clone());
            if expected == ApproveError::NotPending {
                state
                    .notifier()
                    .set_status(&proposal.id, NotificationStatus::Dismissed, None);
            }
            assert_eq!(approve(&state, &proposal.id).await, Err(expected));
        }

        let no_registry = state(&[]);
        notify(&no_registry, notification(NotificationKind::Proposal));
        assert_eq!(
            approve(&no_registry, "p1").await,
            Err(ApproveError::NoRegistry)
        );
    }

    #[tokio::test]
    async fn dismiss_moves_a_pending_notification_to_dismissed_once() {
        let state = state(&[]);
        notify(&state, notification(NotificationKind::Advisory));

        let done = dismiss(&state, "p1").await.unwrap();

        assert_eq!(done.status, NotificationStatus::Dismissed);
        assert_eq!(dismiss(&state, "p1").await, Err(ApproveError::NotPending));
        assert_eq!(dismiss(&state, "nope").await, Err(ApproveError::NotFound));
    }

    #[test]
    fn errors_read_as_user_messages() {
        let all = [
            ApproveError::NotFound,
            ApproveError::NotPending,
            ApproveError::NotProposal,
            ApproveError::UnknownProfile,
            ApproveError::NoRegistry,
            ApproveError::NoDiff,
            ApproveError::NoModel,
            ApproveError::Busy,
        ];
        for error in all {
            let text = error.to_string();
            assert!(!text.is_empty());
            assert!(text.chars().next().unwrap().is_lowercase(), "{text}");
        }
        assert!(
            ApproveError::NoRegistry
                .to_string()
                .contains("TUNER_REGISTRY")
        );
    }
}

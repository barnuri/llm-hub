//! Notifications API: the inbox, approve and dismiss, ingest for hooks, mute rules and the
//! desktop popup setup. Errors answer the shared [`AppError`] body.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};

use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::error_body::ErrorBody;
use crate::schemas::notification::{
    MuteMatch, MuteOrigin, MuteScope, Notification, NotificationKind, NotificationMute,
    NotificationSeverity, NotificationStatus,
};
use crate::services::local_tuner::approve::{self, ApproveError};
use crate::services::notifications::desktop::{DesktopSink, DesktopStatus, fix_steps};
use crate::services::notifications::mute::MuteError;
use crate::services::rate_limiter::{self, NOTIFICATIONS_PER_MINUTE};
use crate::services::store::now_ms;
use crate::utils::hex::to_hex;

/// The longest `source` an ingest call takes, in characters.
const MAX_SOURCE_CHARS: usize = 64;
/// The most bytes title and body of an ingest call hold together.
const MAX_TEXT_BYTES: usize = 4 * 1024;
/// The longest note a mute rule takes, in characters.
const MAX_NOTE_CHARS: usize = 500;

impl From<ApproveError> for AppError {
    fn from(error: ApproveError) -> AppError {
        let message = error.to_string();
        match error {
            ApproveError::NotFound => AppError::NotFound(message),
            ApproveError::NotPending
            | ApproveError::NotProposal
            | ApproveError::UnknownProfile
            | ApproveError::NoRegistry
            | ApproveError::NoDiff
            | ApproveError::NoModel
            | ApproveError::Busy => AppError::Conflict(message),
        }
    }
}

impl From<MuteError> for AppError {
    fn from(error: MuteError) -> AppError {
        let message = error.to_string();
        match error {
            MuteError::EnvRule(_) => AppError::Conflict(message),
            MuteError::TooMany | MuteError::GlobTooLong(_) | MuteError::EmptyMatch => {
                AppError::BadRequest(message)
            }
        }
    }
}

#[derive(Debug, Deserialize, Default, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// `1` also lists the notifications a mute rule with scope `all` hides. Default `0`.
    #[serde(default)]
    pub include_muted: Option<String>,
}

/// The inbox and the count of pending notifications that are not muted.
#[derive(Debug, Serialize, ToSchema)]
pub struct NotificationList {
    /// Newest first.
    pub notifications: Vec<Notification>,
    pub pending: usize,
}

#[utoipa::path(get, path = "/api/notifications", operation_id = "list_notifications", tag = "notifications", summary = "List notifications", params(ListQuery), responses((status = 200, description = "Notifications, newest first", body = NotificationList), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Json<NotificationList> {
    let include_muted = matches!(query.include_muted.as_deref(), Some("1" | "true"));
    let notifier = state.notifier();
    let notifications = notifier
        .list()
        .into_iter()
        .filter(|n| include_muted || !n.muted)
        .collect();
    Json(NotificationList {
        notifications,
        pending: notifier.pending_count(),
    })
}

/// A notification a hook, script or extension sends.
#[derive(Debug, Deserialize, ToSchema)]
pub struct IngestInput {
    /// Who sends it, 1 to 64 characters, for example `ci` or `ext:backup`.
    pub source: String,
    pub severity: NotificationSeverity,
    /// A one-line summary. Title and body hold at most 4 KiB together.
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// `custom` (default), `advisory` or `guard`. A `proposal` needs a flag change, so ingest
    /// refuses it.
    #[serde(default)]
    pub kind: Option<NotificationKind>,
}

#[utoipa::path(post, path = "/api/notifications", tag = "notifications", summary = "Send a notification", description = "Stores the notification and delivers it to the sinks unless a mute or the severity filter stops it. Each source may send 60 per minute.", request_body(content = IngestInput, content_type = "application/json"), responses((status = 200, description = "The stored notification", body = Notification), (status = 202, description = "Notifications are off (`{\"disabled\": true}`); nothing was stored.", body = Object), (status = 400, description = "Invalid source, kind or text.", body = ErrorBody), (status = 429, description = "The source sent more than 60 in the last minute.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn ingest(
    State(state): State<AppState>,
    Json(input): Json<IngestInput>,
) -> Result<Response, AppError> {
    let notification = validate_ingest(input, now_ms())?;
    if !rate_limiter::NOTIFICATIONS.allow(&notification.source, now_ms()) {
        return Err(AppError::TooManyRequests(format!(
            "source {} sent more than {NOTIFICATIONS_PER_MINUTE} notifications in the last minute",
            notification.source
        )));
    }
    Ok(match state.notifier().notify(notification) {
        Some(stored) => Json(stored).into_response(),
        None => (StatusCode::ACCEPTED, Json(json!({ "disabled": true }))).into_response(),
    })
}

fn validate_ingest(input: IngestInput, now_ms: u64) -> Result<Notification, AppError> {
    let source = input.source.trim();
    if source.is_empty() || source.chars().count() > MAX_SOURCE_CHARS {
        return Err(AppError::BadRequest(format!(
            "source must be 1 to {MAX_SOURCE_CHARS} characters"
        )));
    }
    if input.title.trim().is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }
    if input.title.len() + input.body.len() > MAX_TEXT_BYTES {
        return Err(AppError::BadRequest(format!(
            "title and body must hold at most {MAX_TEXT_BYTES} bytes together"
        )));
    }
    let kind = input.kind.unwrap_or(NotificationKind::Custom);
    if kind == NotificationKind::Proposal {
        return Err(AppError::BadRequest(
            "kind proposal needs a flag change; send custom, advisory or guard".into(),
        ));
    }
    Ok(Notification {
        id: format!("api-{}", to_hex(&rand::random::<[u8; 8]>())),
        kind,
        source: source.to_owned(),
        severity: input.severity,
        title: input.title,
        body: input.body,
        profile: input.profile,
        model: input.model,
        rule: None,
        diff: None,
        actions: Notification::default_actions(kind),
        status: NotificationStatus::Pending,
        created_ms: now_ms,
        updated_ms: now_ms,
        result: None,
        muted: false,
    })
}

#[utoipa::path(post, path = "/api/notifications/{id}/approve", tag = "notifications", summary = "Approve a proposal", description = "Edits the profile's model registry and runs its apply command. An apply that fails still answers 200, with status `failed` and the output tail in `result`.", params(("id" = String, Path, description = "Notification id")), responses((status = 200, description = "The notification, `applied` or `failed`", body = Notification), (status = 404, description = "Unknown id.", body = ErrorBody), (status = 409, description = "Not a pending proposal, no registry or diff, unknown profile, or another apply runs.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn approve(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Notification>, AppError> {
    Ok(Json(approve::approve(&state, &id).await?))
}

#[utoipa::path(post, path = "/api/notifications/{id}/dismiss", tag = "notifications", summary = "Dismiss a notification", params(("id" = String, Path, description = "Notification id")), responses((status = 200, description = "The dismissed notification", body = Notification), (status = 404, description = "Unknown id.", body = ErrorBody), (status = 409, description = "The notification is not pending.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn dismiss(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Notification>, AppError> {
    Ok(Json(approve::dismiss(&state, &id).await?))
}

/// Every mute rule, env rules first.
#[derive(Debug, Serialize, ToSchema)]
pub struct MuteList {
    pub mutes: Vec<NotificationMute>,
}

#[utoipa::path(get, path = "/api/notifications/mutes", tag = "notifications", summary = "List mute rules", responses((status = 200, description = "Mute rules, env rules first", body = MuteList), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn list_mutes(State(state): State<AppState>) -> Json<MuteList> {
    Json(MuteList {
        mutes: state.notifier().mutes(),
    })
}

/// A new UI mute rule. The hub sets its id, origin and creation time.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MuteInput {
    pub matcher: MuteMatch,
    /// When the snooze ends, in Unix ms. Absent or `null` mutes forever.
    #[serde(default)]
    pub until_ms: Option<u64>,
    pub scope: MuteScope,
    #[serde(default)]
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/api/notifications/mutes", tag = "notifications", summary = "Add a mute rule", request_body(content = MuteInput, content_type = "application/json"), responses((status = 200, description = "The saved rule", body = NotificationMute), (status = 400, description = "Empty match, glob too long, too many rules, a past `until_ms` or a note too long.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn add_mute(
    State(state): State<AppState>,
    Json(input): Json<MuteInput>,
) -> Result<Json<NotificationMute>, AppError> {
    let now = now_ms();
    if input.until_ms.is_some_and(|until| until <= now) {
        return Err(AppError::BadRequest(
            "until_ms must be in the future".into(),
        ));
    }
    if input
        .note
        .as_ref()
        .is_some_and(|note| note.chars().count() > MAX_NOTE_CHARS)
    {
        return Err(AppError::BadRequest(format!(
            "note must be at most {MAX_NOTE_CHARS} characters"
        )));
    }
    let rule = NotificationMute {
        id: format!("ui-{}", to_hex(&rand::random::<[u8; 8]>())),
        matcher: input.matcher,
        until_ms: input.until_ms,
        scope: input.scope,
        note: input.note,
        origin: MuteOrigin::Ui,
        created_ms: now,
    };
    state
        .notifier()
        .add_mute(rule)
        .map(Json)
        .map_err(AppError::from)
}

#[utoipa::path(delete, path = "/api/notifications/mutes/{id}", tag = "notifications", summary = "Delete a mute rule", params(("id" = String, Path, description = "Mute rule id")), responses((status = 204, description = "Deleted."), (status = 404, description = "Unknown id.", body = ErrorBody), (status = 409, description = "The rule comes from `LLM_HUB_NOTIFY_MUTE`.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn delete_mute(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    match state.notifier().remove_mute(&id) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(AppError::NotFound(format!("mute rule {id} not found"))),
        Err(error) => Err(error.into()),
    }
}

#[utoipa::path(get, path = "/api/notifications/desktop", tag = "notifications", summary = "Desktop popup status", responses((status = 200, description = "Status and fix steps for this OS", body = DesktopStatus), (status = 409, description = "The desktop sink is not set up.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn desktop_status(
    State(state): State<AppState>,
) -> Result<Json<DesktopStatus>, AppError> {
    Ok(Json(desktop_sink(&state)?.status()))
}

#[utoipa::path(post, path = "/api/notifications/desktop/test", tag = "notifications", summary = "Send a test popup", description = "A failed send answers 200 too; `last_error` then holds the reason.", responses((status = 200, description = "Status after the send", body = DesktopStatus), (status = 409, description = "The desktop sink is not set up.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn desktop_test(State(state): State<AppState>) -> Result<Json<DesktopStatus>, AppError> {
    let sink = desktop_sink(&state)?;
    // The sink records the outcome in `last_error`, which the status carries.
    if let Err(error) = sink.send_test().await {
        tracing::debug!("desktop test popup failed: {error}");
    }
    Ok(Json(sink.status()))
}

#[utoipa::path(post, path = "/api/notifications/desktop/confirm", tag = "notifications", summary = "Confirm the test popup showed", responses((status = 200, description = "Status with `confirmed` set", body = DesktopStatus), (status = 409, description = "The desktop sink is not set up.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn desktop_confirm(
    State(state): State<AppState>,
) -> Result<Json<DesktopStatus>, AppError> {
    let sink = desktop_sink(&state)?;
    sink.confirm();
    Ok(Json(sink.status()))
}

#[utoipa::path(post, path = "/api/notifications/desktop/fix/{step_id}", tag = "notifications", summary = "Run a desktop fix step", params(("step_id" = String, Path, description = "Fix step id from the status `steps`")), responses((status = 200, description = "Status after the step ran", body = DesktopStatus), (status = 400, description = "Unknown or copy-only step.", body = ErrorBody), (status = 409, description = "The desktop sink is not set up.", body = ErrorBody), (status = 500, description = "The step's command failed.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn desktop_fix(
    State(state): State<AppState>,
    Path(step_id): Path<String>,
) -> Result<Json<DesktopStatus>, AppError> {
    let sink = desktop_sink(&state)?;
    let runnable = fix_steps(sink.status().os)
        .iter()
        .any(|step| step.id == step_id && step.runnable);
    if !runnable {
        return Err(AppError::BadRequest(format!(
            "fix step {step_id} is unknown or copy-only"
        )));
    }
    sink.run_fix(&step_id).await.map_err(AppError::Internal)?;
    Ok(Json(sink.status()))
}

fn desktop_sink(state: &AppState) -> Result<Arc<DesktopSink>, AppError> {
    state.notifier().desktop().ok_or_else(|| {
        AppError::Conflict("the desktop sink is not set up; notifications may be off".into())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use serde_json::Value;

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::notification::FlagDiff;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::local_tuner::command::CommandResult;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::notifications::desktop::{DesktopOs, Runner};

    /// Answers every command with `ok` and spawns nothing.
    #[derive(Debug)]
    struct FakeRunner {
        ok: bool,
    }

    impl Runner for FakeRunner {
        fn run<'a>(
            &'a self,
            _argv: &'a [String],
            _stdin: Option<Vec<u8>>,
            _timeout: Duration,
        ) -> BoxFuture<'a, CommandResult> {
            let ok = self.ok;
            Box::pin(async move {
                CommandResult {
                    ok,
                    tail: if ok {
                        String::new()
                    } else {
                        "tool failed".into()
                    },
                }
            })
        }
    }

    fn state(notifications: &str, mutes: &str) -> AppState {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "local"),
            ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
            ("LLM_HUB_NOTIFICATIONS", notifications),
            ("LLM_HUB_NOTIFY_MUTE", mutes),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    async fn serve(state: AppState) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::build_router(state);
        tokio::spawn(async move { axum::serve(listener, router).await });
        base
    }

    fn set_desktop(state: &AppState, os: DesktopOs, ok: bool) {
        let sink = DesktopSink::with_parts(
            true,
            os,
            Arc::new(FakeRunner { ok }),
            Duration::from_secs(1),
            |_| None,
        );
        assert!(state.notifier().set_desktop(Arc::new(sink)));
    }

    fn proposal(id: &str) -> Notification {
        Notification {
            id: id.into(),
            kind: NotificationKind::Proposal,
            source: "local_tuner".into(),
            severity: NotificationSeverity::Warning,
            title: "Raise ctx".into(),
            body: "evidence".into(),
            profile: Some("local".into()),
            model: Some("gemma".into()),
            rule: Some("ctx".into()),
            diff: Some(FlagDiff {
                ctx: Some(16_384),
                ..FlagDiff::default()
            }),
            actions: Notification::default_actions(NotificationKind::Proposal),
            status: NotificationStatus::Pending,
            created_ms: 1,
            updated_ms: 1,
            result: None,
            muted: false,
        }
    }

    async fn post(base: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{base}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn get(base: &str, path: &str) -> (StatusCode, Value) {
        let response = reqwest::get(format!("{base}{path}")).await.unwrap();
        let status = response.status();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    fn ingest_body(source: &str) -> Value {
        json!({"source": source, "severity": "warning", "title": "Disk", "body": "almost full"})
    }

    #[tokio::test]
    async fn ingest_lists_and_hides_muted_unless_asked() {
        let state = state("true", "source=quiet-src");
        let base = serve(state).await;

        let (status, stored) = post(&base, "/api/notifications", ingest_body("list-src")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stored["kind"], "custom");
        assert_eq!(stored["actions"], json!(["dismiss"]));
        assert!(stored["id"].as_str().unwrap().starts_with("api-"));
        let (_, muted) = post(&base, "/api/notifications", ingest_body("quiet-src")).await;
        assert_eq!(muted["muted"], true);

        let (status, listed) = get(&base, "/api/notifications").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["notifications"].as_array().unwrap().len(), 1);
        assert_eq!(listed["pending"], 1);
        let (_, all) = get(&base, "/api/notifications?include_muted=1").await;
        assert_eq!(all["notifications"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ingest_refuses_bad_input_with_the_shared_error_body() {
        let base = serve(state("true", "")).await;
        let cases = [
            json!({"source": " ", "severity": "info", "title": "t", "body": "b"}),
            json!({"source": "x".repeat(65), "severity": "info", "title": "t", "body": "b"}),
            json!({"source": "bad-src", "severity": "info", "title": " ", "body": "b"}),
            json!({"source": "bad-src", "severity": "info", "title": "t", "body": "b".repeat(4_096)}),
            json!({"source": "bad-src", "severity": "info", "title": "t", "body": "b", "kind": "proposal"}),
        ];
        for body in cases {
            let (status, error) = post(&base, "/api/notifications", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(error["error"]["message"].is_string(), "{error}");
        }
        let (status, stored) = post(
            &base,
            "/api/notifications",
            json!({"source": "bad-src", "severity": "critical", "title": "t", "body": "b", "kind": "advisory", "profile": "local", "model": "m"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stored["kind"], "advisory");
        assert_eq!(stored["profile"], "local");
    }

    #[tokio::test]
    async fn ingest_answers_202_when_notifications_are_off() {
        let base = serve(state("false", "")).await;
        let (status, body) = post(&base, "/api/notifications", ingest_body("off-src")).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body, json!({"disabled": true}));
    }

    #[tokio::test]
    async fn ingest_limits_each_source_to_sixty_a_minute() {
        let base = serve(state("true", "")).await;
        let (status, _) = post(&base, "/api/notifications", ingest_body("burst-src")).await;
        assert_eq!(status, StatusCode::OK);
        // Drains the bucket here, then posts until a refusal: a slow call may refill one token.
        let mut refused = None;
        for _ in 0..20 {
            while rate_limiter::NOTIFICATIONS.allow("burst-src", now_ms()) {}
            let (status, error) = post(&base, "/api/notifications", ingest_body("burst-src")).await;
            if status == StatusCode::TOO_MANY_REQUESTS {
                refused = Some(error);
                break;
            }
        }
        let error = refused.expect("ingest never answered 429");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("burst-src")
        );
        assert_eq!(error["error"]["type"], "rate_limit_error");
        let (status, _) = post(&base, "/api/notifications", ingest_body("other-src")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn approve_and_dismiss_map_the_errors() {
        let state = state("true", "");
        state.notifier().notify(proposal("p1"));
        state.notifier().notify(proposal("p2"));
        let base = serve(state).await;

        let (status, error) = post(&base, "/api/notifications/nope/approve", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["message"], "notification not found");
        assert_eq!(error["error"]["type"], "not_found_error");
        let (status, error) = post(&base, "/api/notifications/p1/approve", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("registry")
        );

        let (status, dismissed) = post(&base, "/api/notifications/p1/dismiss", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(dismissed["status"], "dismissed");
        let (status, _) = post(&base, "/api/notifications/p1/dismiss", json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = post(&base, "/api/notifications/nope/dismiss", json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn every_approve_error_but_not_found_is_a_conflict() {
        let cases = [
            (ApproveError::NotFound, StatusCode::NOT_FOUND),
            (ApproveError::NotPending, StatusCode::CONFLICT),
            (ApproveError::NotProposal, StatusCode::CONFLICT),
            (ApproveError::UnknownProfile, StatusCode::CONFLICT),
            (ApproveError::NoRegistry, StatusCode::CONFLICT),
            (ApproveError::NoDiff, StatusCode::CONFLICT),
            (ApproveError::NoModel, StatusCode::CONFLICT),
            (ApproveError::Busy, StatusCode::CONFLICT),
        ];
        for (error, status) in cases {
            let message = error.to_string();
            let mapped = AppError::from(error);
            assert_eq!(mapped.status(), status);
            assert_eq!(mapped.message(), message);
        }
    }

    #[tokio::test]
    async fn mutes_add_list_and_delete() {
        let base = serve(state("true", "source=env-src")).await;
        let client = reqwest::Client::new();

        let (status, rule) = post(
            &base,
            "/api/notifications/mutes",
            json!({"matcher": {"source": "ci"}, "scope": "sinks", "note": "noisy"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rule["origin"], "ui");
        assert!(rule["id"].as_str().unwrap().starts_with("ui-"));
        assert!(rule["created_ms"].as_u64().unwrap() > 0);

        let (status, listed) = get(&base, "/api/notifications/mutes").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["mutes"].as_array().unwrap().len(), 2);
        assert_eq!(listed["mutes"][0]["origin"], "env");
        let env_id = listed["mutes"][0]["id"].as_str().unwrap().to_owned();

        let delete = |id: String| {
            let client = client.clone();
            let url = format!("{base}/api/notifications/mutes/{id}");
            async move { client.delete(url).send().await.unwrap() }
        };
        let deleted = delete(rule["id"].as_str().unwrap().to_owned()).await;
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        let missing = delete(rule["id"].as_str().unwrap().to_owned()).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let env = delete(env_id).await;
        assert_eq!(env.status(), StatusCode::CONFLICT);
        let body: Value = env.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("LLM_HUB_NOTIFY_MUTE")
        );
    }

    #[tokio::test]
    async fn mutes_refuse_bad_rules() {
        let base = serve(state("true", "")).await;
        let cases = [
            json!({"matcher": {}, "scope": "all"}),
            json!({"matcher": {"source": "x".repeat(257)}, "scope": "all"}),
            json!({"matcher": {"source": "ci"}, "scope": "all", "until_ms": 1}),
            json!({"matcher": {"source": "ci"}, "scope": "all", "note": "n".repeat(501)}),
        ];
        for body in cases {
            let (status, error) = post(&base, "/api/notifications/mutes", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(error["error"]["message"].is_string(), "{error}");
        }
    }

    #[tokio::test]
    async fn desktop_routes_need_the_shared_sink() {
        let base = serve(state("true", "")).await;
        let (status, error) = get(&base, "/api/notifications/desktop").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(error["error"]["message"].is_string());
        for path in ["test", "confirm", "fix/send_test"] {
            let (status, _) = post(
                &base,
                &format!("/api/notifications/desktop/{path}"),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{path}");
        }
    }

    #[tokio::test]
    async fn desktop_status_test_confirm_and_fix() {
        let state = state("true", "");
        set_desktop(&state, DesktopOs::MacOs, true);
        let base = serve(state).await;

        let (status, desktop) = get(&base, "/api/notifications/desktop").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(desktop["os"], "macos");
        assert_eq!(desktop["confirmed"], false);

        let (status, tested) = post(&base, "/api/notifications/desktop/test", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(tested["last_ok_ms"].as_u64().is_some());
        let (_, confirmed) = post(&base, "/api/notifications/desktop/confirm", json!({})).await;
        assert_eq!(confirmed["confirmed"], true);

        let (status, _) = post(
            &base,
            "/api/notifications/desktop/fix/open_settings",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for step in ["focus_off", "rm-rf"] {
            let (status, error) = post(
                &base,
                &format!("/api/notifications/desktop/fix/{step}"),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{step}");
            assert!(error["error"]["message"].as_str().unwrap().contains(step));
        }
    }

    #[tokio::test]
    async fn desktop_failures_show_in_the_status_or_as_500() {
        let state = state("true", "");
        set_desktop(&state, DesktopOs::MacOs, false);
        let base = serve(state).await;

        let (status, tested) = post(&base, "/api/notifications/desktop/test", json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(tested["last_error"], "tool failed");
        let (status, error) = post(
            &base,
            "/api/notifications/desktop/fix/open_settings",
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error["error"]["message"], "tool failed");
    }
}

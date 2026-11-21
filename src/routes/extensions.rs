//! Extensions API: list the running extensions and rules, and turn them on or off. A toggle
//! rebuilds the extension set once, so requests never check it.

use axum::Json;
use axum::extract::{Path, State};

use crate::dependencies::state::AppState;
use crate::schemas::app_error::AppError;
use crate::schemas::error_body::ErrorBody;
use crate::schemas::extension_status::{ExtensionList, ExtensionToggleInput};
use crate::services::extension_toggles::{ToggleError, ToggleTarget};

impl From<ToggleError> for AppError {
    fn from(error: ToggleError) -> AppError {
        match error {
            ToggleError::Unknown(name) => {
                AppError::NotFound(format!("no extension or rule is named `{name}`"))
            }
            ToggleError::SetByEnv(name) => AppError::Conflict(format!(
                "LLM_HUB_EXTENSIONS_DISABLED turns `{name}` off; change it in the env"
            )),
            ToggleError::Store(message) => AppError::Internal(message),
        }
    }
}

#[utoipa::path(get, path = "/api/extensions", operation_id = "list_extensions", tag = "extensions", summary = "List extensions and rules", description = "Every extension in run order, then the hooks, with the rules of the rules file. `disabled_by` says whether the env or the UI turned one off.", responses((status = 200, description = "Extensions and rules", body = ExtensionList), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn list(State(state): State<AppState>) -> Json<ExtensionList> {
    Json(ExtensionList {
        extensions: state.extension_statuses(),
    })
}

#[utoipa::path(put, path = "/api/extensions/{name}", operation_id = "set_extension", tag = "extensions", summary = "Turn an extension on or off", description = "Saves the choice in the store, so it survives a restart. The hooks of the rules file go by the name `hooks`.", params(("name" = String, Path, description = "Extension name, for example `upstream-limits`, `rules` or `hooks`.")), request_body(content = ExtensionToggleInput, content_type = "application/json"), responses((status = 200, description = "Extensions and rules after the change", body = ExtensionList), (status = 404, description = "No extension has that name.", body = ErrorBody), (status = 409, description = "`LLM_HUB_EXTENSIONS_DISABLED` turns it off.", body = ErrorBody), (status = 500, description = "The store could not save the choice.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn set_extension(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<ExtensionToggleInput>,
) -> Result<Json<ExtensionList>, AppError> {
    toggle(state, ToggleTarget::Extension, name, input.enabled).await
}

#[utoipa::path(put, path = "/api/extensions/rules/{rule}", operation_id = "set_extension_rule", tag = "extensions", summary = "Turn one rule on or off", description = "Turns one rule of the rules file on or off, and saves the choice in the store.", params(("rule" = String, Path, description = "Rule name from the rules file.")), request_body(content = ExtensionToggleInput, content_type = "application/json"), responses((status = 200, description = "Extensions and rules after the change", body = ExtensionList), (status = 404, description = "No rule has that name.", body = ErrorBody), (status = 409, description = "`LLM_HUB_EXTENSIONS_DISABLED` turns it off with `rules:<name>`.", body = ErrorBody), (status = 500, description = "The store could not save the choice.", body = ErrorBody), (status = 401, description = "Missing or invalid hub key.", body = ErrorBody)))]
pub async fn set_rule(
    State(state): State<AppState>,
    Path(rule): Path<String>,
    Json(input): Json<ExtensionToggleInput>,
) -> Result<Json<ExtensionList>, AppError> {
    toggle(state, ToggleTarget::Rule, rule, input.enabled).await
}

/// Applies the toggle on a blocking thread, because it writes to the store.
async fn toggle(
    state: AppState,
    target: ToggleTarget,
    name: String,
    enabled: bool,
) -> Result<Json<ExtensionList>, AppError> {
    let applied = state.clone();
    tokio::task::spawn_blocking(move || applied.toggle_extension(target, &name, enabled))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(list(State(state)).await)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    use super::*;
    use crate::configs::HubConfig;
    use crate::schemas::extension_status::DisabledBy;
    use crate::services::extension_toggles::sample_set;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    fn state(disabled: &str) -> AppState {
        let vars = HashMap::from([(
            "LLM_HUB_EXTENSIONS_DISABLED".to_owned(),
            disabled.to_owned(),
        )]);
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        state.set_extensions(sample_set());
        state
    }

    fn input(enabled: bool) -> Json<ExtensionToggleInput> {
        Json(ExtensionToggleInput { enabled })
    }

    fn status(result: Result<Json<ExtensionList>, AppError>) -> StatusCode {
        match result {
            Ok(_) => StatusCode::OK,
            Err(error) => error.into_response().status(),
        }
    }

    #[tokio::test]
    async fn list_shows_every_extension_with_its_rules() {
        let Json(list) = list(State(state("upstream-limits"))).await;

        assert_eq!(list.extensions.len(), 7);
        assert_eq!(list.extensions[3].disabled_by, Some(DisabledBy::Env));
        assert_eq!(list.extensions[5].rules.as_ref().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn toggles_answer_200_404_and_409() {
        let state = state("upstream-limits,rules:trial");

        let Json(off) = set_extension(State(state.clone()), Path("team-gate".into()), input(false))
            .await
            .unwrap();
        assert_eq!(off.extensions[4].disabled_by, Some(DisabledBy::Ui));
        assert!(!state.extensions().names().contains(&"team-gate"));
        let Json(rule_off) = set_rule(State(state.clone()), Path("tag".into()), input(false))
            .await
            .unwrap();
        let rules = rule_off.extensions[5].rules.as_ref().unwrap();
        assert_eq!(rules[1].disabled_by, Some(DisabledBy::Ui));

        for (result, expected) in [
            (
                set_extension(State(state.clone()), Path("nope".into()), input(false)).await,
                StatusCode::NOT_FOUND,
            ),
            (
                set_rule(State(state.clone()), Path("nope".into()), input(true)).await,
                StatusCode::NOT_FOUND,
            ),
            (
                set_extension(
                    State(state.clone()),
                    Path("upstream-limits".into()),
                    input(true),
                )
                .await,
                StatusCode::CONFLICT,
            ),
            (
                set_rule(State(state.clone()), Path("trial".into()), input(true)).await,
                StatusCode::CONFLICT,
            ),
            (
                set_extension(State(state.clone()), Path("team-gate".into()), input(true)).await,
                StatusCode::OK,
            ),
        ] {
            assert_eq!(status(result), expected);
        }
        assert!(state.extensions().names().contains(&"team-gate"));
    }

    #[test]
    fn a_store_failure_answers_500() {
        let error = AppError::from(ToggleError::Store("disk full".into()));

        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

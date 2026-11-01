use std::cmp::Reverse;
use std::collections::HashMap;

use serde_json::Value;

use crate::dependencies::state::AppState;
use crate::services::copilot_auto::AutoTier;
use crate::services::store::now_ms;

const USAGE_WINDOW_MS: u64 = 30 * 86_400_000;
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

pub(crate) async fn refresh_usage(state: &AppState) {
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || {
        let Some(store) = worker_state.store() else {
            return worker_state.stats().model_request_counts();
        };
        match store.model_request_counts(now_ms().saturating_sub(USAGE_WINDOW_MS)) {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(%error, "model usage refresh failed, using live request counts");
                worker_state.stats().model_request_counts()
            }
        }
    })
    .await;
    match result {
        Ok(counts) => state.set_model_usage(counts),
        Err(error) => tracing::warn!(%error, "model usage worker failed"),
    }
}

pub(crate) fn sort_models(payload: &mut Value, state: &AppState) {
    let Some(models) = payload.get_mut("data").and_then(Value::as_array_mut) else {
        return;
    };
    sort_by_usage(models, &state.model_usage());
}

fn sort_by_usage(models: &mut [Value], usage: &HashMap<String, u64>) {
    models.sort_by_cached_key(|model| {
        let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
        let lookup = id.strip_suffix(LONG_CONTEXT_SUFFIX).unwrap_or(id);
        let tier = lookup
            .split_once('/')
            .and_then(|(_, bare)| AutoTier::from_model(bare));
        let requests = if tier.is_some() {
            0
        } else {
            usage.get(lookup).copied().unwrap_or(0)
        };
        let tier_order = tier.and_then(|tier| {
            AutoTier::ALL
                .iter()
                .position(|candidate| *candidate == tier)
        });
        let profile = if tier.is_some() {
            id.split_once('/').map_or("", |(profile, _)| profile)
        } else {
            ""
        };
        (
            tier.is_none(),
            Reverse(requests),
            profile.to_string(),
            tier_order,
            id.to_string(),
        )
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn auto_choices_precede_usage_ranked_models_with_deterministic_ties() {
        let mut models = vec![
            json!({"id": "copilot/zero-z"}),
            json!({"id": "copilot/auto-turbo"}),
            json!({"id": "copilot/fusion"}),
            json!({"id": "copilot/opus[1m]"}),
            json!({"id": "local/qwen"}),
            json!({"id": "copilot/auto-intelligence"}),
            json!({"id": "copilot/auto"}),
            json!({"id": "copilot/auto-efficiency"}),
            json!({"id": "other/auto"}),
        ];
        let usage = HashMap::from([
            ("copilot/opus".to_string(), 40),
            ("local/qwen".to_string(), 7),
            ("copilot/auto-turbo".to_string(), 100),
        ]);
        sort_by_usage(&mut models, &usage);
        let ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "copilot/auto-efficiency",
                "copilot/auto",
                "copilot/auto-intelligence",
                "other/auto",
                "copilot/auto-turbo",
                "copilot/opus[1m]",
                "local/qwen",
                "copilot/fusion",
                "copilot/zero-z",
            ]
        );
        let ordered = models.clone();
        models.reverse();
        sort_by_usage(&mut models, &usage);
        assert_eq!(models, ordered);
    }

    #[tokio::test]
    async fn persistent_counts_are_cached_and_refresh_failures_use_live_counts() {
        use crate::configs::HubConfig;
        use crate::services::fake_gh_cli::FakeGhCli;
        use crate::services::gh_cli::GhOutcome;
        use crate::services::memory_credential_store::MemoryCredentialStore;
        use crate::services::stats::RequestOutcome;
        use crate::services::store::Store;
        use std::sync::Arc;

        let config = HubConfig::from_map(&HashMap::from([
            ("LLM_HUB_PROFILES".into(), "p".into()),
            ("LLM_HUB_P_BASE_URL".into(), "http://127.0.0.1:9".into()),
        ]))
        .unwrap();
        let store = Store::open("sqlite", Some(":memory:")).unwrap();
        let state = AppState::with_seams(
            config,
            Some(store),
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        let mut outcome = RequestOutcome {
            profile: "p".into(),
            model_key: "p/z".into(),
            status: 200,
            ts_ms: now_ms(),
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: None,
        };
        state.store().unwrap().record(&outcome).unwrap();
        refresh_usage(&state).await;
        assert_eq!(state.model_usage().get("p/z"), Some(&1));
        let poisoned = state.clone();
        assert!(
            std::thread::spawn(move || {
                let Store::Sqlite(conn) = poisoned.store().unwrap() else {
                    panic!("expected sqlite");
                };
                let _guard = conn.lock().unwrap();
                panic!("poison store for warm-read regression");
            })
            .join()
            .is_err()
        );
        let mut payload = json!({"data": [{"id": "p/a"}, {"id": "p/z[1m]"}]});
        sort_models(&mut payload, &state);
        assert_eq!(payload["data"][0]["id"], "p/z[1m]");
        outcome.model_key = "p/a".into();
        state.stats().record(&outcome);
        refresh_usage(&state).await;
        assert_eq!(state.model_usage().get("p/a"), Some(&1));
        assert!(!state.model_usage().contains_key("p/z"));
    }

    #[test]
    fn accepted_aliases_belong_to_auto_group() {
        let mut models = vec![
            json!({"id": "p/ordinary"}),
            json!({"id": "p/auto-max"}),
            json!({"id": "p/auto-balanced"}),
            json!({"id": "p/auto-eco"}),
        ];
        sort_by_usage(&mut models, &HashMap::from([("p/ordinary".into(), 99)]));
        assert_eq!(models[0]["id"], "p/auto-eco");
        assert_eq!(models[1]["id"], "p/auto-balanced");
        assert_eq!(models[2]["id"], "p/auto-max");
        assert_eq!(models[3]["id"], "p/ordinary");
    }
}

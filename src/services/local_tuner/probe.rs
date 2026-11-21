//! Background probe: fits prefill curves from request rows, reads llama-swap and runs the rules.
//! It never runs on the request path and never starts a stopped server.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use reqwest::StatusCode;

use super::llama_swap::{self, EffectiveFlags, RunningModel};
use super::prefill::{self, PrefillFit, PrefillSample};
use super::registry;
use super::rules::{self, ClientStats, RuleInput};
use crate::configs::ProfileConfig;
use crate::consts::KEEP_WARM_CLIENT;
use crate::dependencies::state::AppState;
use crate::schemas::notification::Notification;
use crate::services::stats::RequestOutcome;
use crate::services::store::{StatsFilter, now_ms};

/// The probe waits this long after boot before its first run.
pub const FIRST_DELAY: Duration = Duration::from_secs(30);

/// The probe runs once per this interval after the first run.
pub const INTERVAL: Duration = Duration::from_mins(5);

/// The probe reads request rows from this far back, in milliseconds (7 days).
const WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// The llama.cpp log line that says a model ignores `--cache-reuse`.
const CACHE_REUSE_UNSUPPORTED: &str = "cache_reuse is not supported";

/// The server flags the tuner rules read, with their short aliases.
const RULE_FLAGS: [&str; 5] = [
    "--cache-type-k",
    "-ctk",
    "--cache-type-v",
    "-ctv",
    "--cache-reuse",
];

/// nginx's "client closed request" status, which some proxies record for a cancelled call.
const CLIENT_CLOSED_REQUEST: u16 = 499;

/// Starts the loop: a first run after [`FIRST_DELAY`], then one per [`INTERVAL`]. Each tick reads
/// the live config, so `LLM_HUB_LOCAL_TUNER` takes effect without a restart.
pub fn spawn_probe_loop(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_DELAY).await;
        let mut ticks = tokio::time::interval(INTERVAL);
        loop {
            ticks.tick().await;
            run_once(&state, now_ms()).await;
        }
    })
}

/// Runs one probe at `now_ms` and returns the notifications the inbox took. A failed read logs
/// at debug and drops only that piece of evidence.
pub async fn run_once(state: &AppState, now_ms: u64) -> Vec<Notification> {
    let config = state.config();
    if !config.local_tuner_enabled {
        return Vec::new();
    }
    let profiles: Vec<&ProfileConfig> = config
        .profiles
        .iter()
        .filter(|p| p.enabled && p.is_local())
        .collect();
    let names = profiles.iter().map(|p| p.name.clone()).collect();
    let evidence = read_store(state, names, now_ms.saturating_sub(WINDOW_MS)).await;
    let fits = fit_models(evidence.values().flat_map(|e| &e.rows));
    state.local_tuner().set_fits(fits.clone());

    let no_rows = ProfileRows::default();
    let mut raised = Vec::new();
    for profile in profiles {
        if profile.managed.is_some() && !state.managed().is_running(&profile.name) {
            tracing::debug!(
                "tuner probe skips {}: its process is not running",
                profile.name
            );
            continue;
        }
        let Some(client) = state.client(&profile.name) else {
            continue;
        };
        let rows = evidence.get(&profile.name).unwrap_or(&no_rows);
        let server = read_server(&client, llama_swap::root_of(&profile.base_url)).await;
        let models = models_of(&profile.name, rows, &server);
        let registry_ctx = read_registry_ctx(profile.tuner.registry.clone(), &models).await;
        let plain_ctx = server.plain_ctx.filter(|_| models.len() == 1);
        for model in &models {
            let ready = server.ready.get(model);
            let ctx = registry_ctx
                .get(model)
                .copied()
                .or_else(|| ready.and_then(|r| r.ctx))
                .or(plain_ctx);
            let input = rule_input(profile, model, rows, &fits, ready, &server, ctx);
            let mut found = rules::evaluate(&input, now_ms);
            if ready.is_none() {
                // Without the server's command line, a flag proposal would guess at the flags.
                found.retain(|n| n.diff.as_ref().is_none_or(|d| d.add.is_empty()));
            }
            raised.extend(found.into_iter().filter_map(|n| state.notifier().notify(n)));
        }
    }
    raised
}

/// Request rows and tool shares of one profile.
#[derive(Debug, Default)]
struct ProfileRows {
    /// Rows of the window, keep-warm pings removed.
    rows: Vec<RequestOutcome>,
    /// Qualified model to the share of its prompt bytes that are tool definitions.
    tool_share: HashMap<String, f64>,
}

/// What one profile's server said in this tick.
#[derive(Debug, Default)]
struct ServerEvidence {
    /// Ready llama-swap model to its flags and served context.
    ready: HashMap<String, ReadyModel>,
    /// The context of a plain llama-server that answers 404 on `/running`.
    plain_ctx: Option<u64>,
    /// True when the server log says a model ignores `--cache-reuse`.
    cache_reuse_unsupported: bool,
}

#[derive(Debug)]
struct ReadyModel {
    flags: EffectiveFlags,
    ctx: Option<u64>,
}

async fn read_store(
    state: &AppState,
    profiles: Vec<String>,
    since_ms: u64,
) -> HashMap<String, ProfileRows> {
    let state = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        let Some(store) = state.store() else {
            return HashMap::new();
        };
        profiles
            .into_iter()
            .map(|name| {
                let filter = StatsFilter {
                    since_ms: Some(since_ms),
                    profile: Some(name.clone()),
                    ..StatsFilter::default()
                };
                let mut rows = store.filtered_outcomes(&filter).unwrap_or_else(|e| {
                    tracing::debug!("tuner probe cannot read rows of {name}: {e}");
                    Vec::new()
                });
                rows.retain(|r| r.client.as_deref() != Some(KEEP_WARM_CLIENT));
                let tool_share = store
                    .prompt_tool_share(&name, since_ms)
                    .unwrap_or_else(|e| {
                        tracing::debug!("tuner probe cannot read tool shares of {name}: {e}");
                        HashMap::new()
                    });
                (name, ProfileRows { rows, tool_share })
            })
            .collect()
    })
    .await;
    read.unwrap_or_else(|e| {
        tracing::debug!("tuner probe store read failed: {e}");
        HashMap::new()
    })
}

/// Fits each qualified model on its successful rows that measured a first byte.
fn fit_models<'a>(rows: impl Iterator<Item = &'a RequestOutcome>) -> HashMap<String, PrefillFit> {
    let mut samples: HashMap<&str, Vec<PrefillSample>> = HashMap::new();
    for row in rows.filter(|r| is_success(r.status)) {
        let Some(ttft_ms) = row.ttft_ms else {
            continue;
        };
        let tokens = row.tokens_in.saturating_sub(row.cache_read_tokens);
        if tokens == 0 {
            continue;
        }
        samples
            .entry(row.model_key.as_str())
            .or_default()
            .push(PrefillSample {
                tokens,
                prefill_ms: ttft_ms,
            });
    }
    samples
        .into_iter()
        .filter_map(|(model, samples)| prefill::fit(&samples).map(|fit| (model.to_owned(), fit)))
        .collect()
}

async fn read_server(client: &reqwest::Client, root: &str) -> ServerEvidence {
    let mut evidence = ServerEvidence::default();
    match llama_swap::running(client, root).await {
        Ok(running) => {
            for model in running.into_iter().filter(RunningModel::is_ready) {
                let ctx = match llama_swap::props(client, root, Some(&model.model)).await {
                    Ok(props) => props.n_ctx,
                    Err(e) => {
                        tracing::debug!("tuner probe cannot read props of {}: {e}", model.model);
                        None
                    }
                };
                let flags = EffectiveFlags::parse(&model.cmd);
                evidence
                    .ready
                    .insert(model.model, ReadyModel { flags, ctx });
            }
            if !evidence.ready.is_empty() {
                evidence.cache_reuse_unsupported =
                    llama_swap::log_mentions(client, root, CACHE_REUSE_UNSUPPORTED)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::debug!("tuner probe cannot read logs of {root}: {e}");
                            false
                        });
            }
        }
        Err(e) if e.is_not_found() => match llama_swap::props(client, root, None).await {
            Ok(props) => evidence.plain_ctx = props.n_ctx,
            Err(e) => tracing::debug!("tuner probe cannot read props of {root}: {e}"),
        },
        Err(e) => tracing::debug!("tuner probe cannot read {root}/running: {e}"),
    }
    evidence
}

/// Bare model ids of `profile` seen in its rows or ready on its server, sorted.
fn models_of(profile: &str, rows: &ProfileRows, server: &ServerEvidence) -> BTreeSet<String> {
    let prefix = format!("{profile}/");
    rows.rows
        .iter()
        .filter_map(|r| r.model_key.strip_prefix(&prefix))
        .map(str::to_owned)
        .chain(server.ready.keys().cloned())
        .collect()
}

/// Registry ctx per model, read off the async runtime. Empty without a registry.
async fn read_registry_ctx(
    path: Option<PathBuf>,
    models: &BTreeSet<String>,
) -> HashMap<String, u64> {
    let Some(path) = path else {
        return HashMap::new();
    };
    let models = models.clone();
    tokio::task::spawn_blocking(move || {
        models
            .into_iter()
            .filter_map(|model| registry::ctx_of(&path, &model).map(|ctx| (model, ctx)))
            .collect()
    })
    .await
    .unwrap_or_else(|e| {
        tracing::debug!("tuner probe registry read failed: {e}");
        HashMap::new()
    })
}

fn rule_input(
    profile: &ProfileConfig,
    model: &str,
    rows: &ProfileRows,
    fits: &HashMap<String, PrefillFit>,
    ready: Option<&ReadyModel>,
    server: &ServerEvidence,
    ctx: Option<u64>,
) -> RuleInput {
    let qualified = format!("{}/{model}", profile.name);
    let mut faster_models: Vec<(String, PrefillFit)> = fits
        .iter()
        .filter(|(name, _)| **name != qualified)
        .map(|(name, fit)| (name.clone(), *fit))
        .collect();
    faster_models.sort_by(|a, b| a.0.cmp(&b.0));
    RuleInput {
        profile: profile.name.clone(),
        model: model.to_owned(),
        budget_ms: budget_ms(profile),
        fit: fits.get(&qualified).copied(),
        flags: ready.map(|r| rule_flags(&r.flags)).unwrap_or_default(),
        cache_reuse_unsupported: ready.is_some() && server.cache_reuse_unsupported,
        ctx,
        tool_share: rows.tool_share.get(&qualified).copied(),
        clients: client_stats(rows.rows.iter().filter(|r| r.model_key == qualified)),
        faster_models,
    }
}

/// The prefill guard budget, else the profile's first-byte limit, else its total timeout; 0 is none.
fn budget_ms(profile: &ProfileConfig) -> u64 {
    profile
        .tuner
        .prefill_guard
        .or(profile.first_byte_timeout_ms.filter(|ms| *ms > 0))
        .or(profile.timeout_ms)
        .unwrap_or(0)
}

/// The flags the rules read, copied out of the parsed command line.
fn rule_flags(flags: &EffectiveFlags) -> HashMap<String, Option<String>> {
    RULE_FLAGS
        .iter()
        .filter(|flag| flags.has(flag))
        .map(|flag| ((*flag).to_owned(), flags.get(flag).map(str::to_owned)))
        .collect()
}

/// One entry per client signature, sorted by client.
fn client_stats<'a>(rows: impl Iterator<Item = &'a RequestOutcome>) -> Vec<ClientStats> {
    let mut by_client: BTreeMap<&str, Vec<&RequestOutcome>> = BTreeMap::new();
    for row in rows {
        if let Some(client) = row.client.as_deref() {
            by_client.entry(client).or_default().push(row);
        }
    }
    by_client
        .into_iter()
        .map(|(client, rows)| {
            let failed = rows.iter().filter(|r| is_failed(r)).count();
            #[allow(
                clippy::cast_precision_loss,
                reason = "request counts stay far below 2^52"
            )]
            let failed_or_timeout_share = failed as f64 / rows.len() as f64;
            ClientStats {
                client: client.to_owned(),
                requests: rows.len() as u64,
                median_prompt_tokens: median(rows.iter().map(|r| r.tokens_in).collect())
                    .unwrap_or(0),
                median_ttft_ms: median(rows.iter().filter_map(|r| r.ttft_ms).collect()),
                failed_or_timeout_share,
            }
        })
        .collect()
}

fn is_success(status: u16) -> bool {
    StatusCode::from_u16(status).is_ok_and(|s| s.is_success())
}

/// A server error, a timeout, a cancelled call, or a success that sent nothing: the client gave
/// up before the first byte.
fn is_failed(row: &RequestOutcome) -> bool {
    let Ok(status) = StatusCode::from_u16(row.status) else {
        return false;
    };
    status.is_server_error()
        || status == StatusCode::REQUEST_TIMEOUT
        || status.as_u16() == CLIENT_CLOSED_REQUEST
        || (status.is_success() && row.ttft_ms.is_none() && row.tokens_out == 0)
}

/// The middle value, the upper one of the two for an even count; `None` when empty.
fn median(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mid = values.len() / 2;
    Some(*values.select_nth_unstable(mid).1)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;
    use crate::services::store::Store;

    const GEMMA_CMD: &str = "llama-server --model /m/gemma.gguf --ctx-size 65536 --cache-reuse 256";
    const Q8_CMD: &str = "llama-server --model /m/qwen.gguf -ctk q8_0 -ctv q8_0";

    fn state(vars: &[(&str, &str)], store: Option<Store>) -> AppState {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            store,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    fn row(model: &str, tokens_in: u64, ttft_ms: Option<u64>, status: u16) -> RequestOutcome {
        RequestOutcome {
            profile: "local".into(),
            model_key: format!("local/{model}"),
            status,
            ttft_ms,
            tokens_in,
            tokens_out: 10,
            client: Some("opencode/1.0".into()),
            ..RequestOutcome::default()
        }
    }

    /// Rows that fit gemma at 10 ms per token: 20k tokens need 200 s.
    fn seed_slow_gemma(store: &Store) {
        for tokens in [1_000, 1_500, 3_000, 6_000, 12_000] {
            store
                .record(&row("gemma", tokens, Some(tokens * 10), 200))
                .unwrap();
        }
        let mut warm = row("gemma", 50_000, Some(1), 200);
        warm.client = Some(KEEP_WARM_CLIENT.into());
        store.record(&warm).unwrap();
    }

    fn sqlite() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open("sqlite", dir.path().join("hub.db").to_str()).unwrap();
        (dir, store)
    }

    async fn llama_swap(running: serde_json::Value, logs: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/running"))
            .respond_with(ResponseTemplate::new(200).set_body_json(running))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/upstream/gemma/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"default_generation_settings": {"n_ctx": 65536}}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(logs.to_owned()))
            .mount(&server)
            .await;
        server
    }

    fn rules_of(found: &[Notification]) -> Vec<String> {
        let mut rules: Vec<String> = found.iter().filter_map(|n| n.rule.clone()).collect();
        rules.sort();
        rules
    }

    #[tokio::test]
    async fn ready_model_gets_flag_proposals_and_a_slow_fit_a_verdict() {
        let server = llama_swap(
            serde_json::json!({"running": [
                {"model": "gemma", "state": "ready", "cmd": GEMMA_CMD},
                {"model": "qwen", "state": "ready", "cmd": Q8_CMD},
                {"model": "llama", "state": "starting", "cmd": "llama-server"}
            ]}),
            "srv load: cache_reuse is not supported by this context",
        )
        .await;
        let (_dir, store) = sqlite();
        seed_slow_gemma(&store);
        let base = format!("{}/v1", server.uri());
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &base),
                ("LLM_HUB_LOCAL_PREFILL_GUARD", "60000"),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            Some(store),
        );

        let found = run_once(&state, now_ms()).await;

        assert_eq!(
            rules_of(&found),
            [
                "cache_reuse_unsupported",
                "ctx_too_large",
                "does_not_fit",
                "kv_cache_q8"
            ]
        );
        assert!(found.iter().all(|n| n.model.as_deref() == Some("gemma")));
        let fit = state.local_tuner().fit("local/gemma").unwrap();
        assert!(!fit.low_confidence);
        assert!(fit.max_tokens_seen <= 12_000, "{fit:?}");
        assert_eq!(state.notifier().list().len(), 4);
        // A second tick keeps the same ids, so the inbox holds no duplicates.
        run_once(&state, now_ms() + 1).await;
        assert_eq!(state.notifier().list().len(), 4);
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.url.path() != "/upstream/llama/props")
        );
    }

    #[tokio::test]
    async fn model_without_a_command_line_gets_no_flag_proposal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"default_generation_settings": {"n_ctx": 131_072}}),
            ))
            .mount(&server)
            .await;
        let (_dir, store) = sqlite();
        seed_slow_gemma(&store);
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
                ("LLM_HUB_LOCAL_TIMEOUT_MS", "60000"),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            Some(store),
        );

        let found = run_once(&state, now_ms()).await;

        assert_eq!(rules_of(&found), ["ctx_too_large", "does_not_fit"]);
        let ctx = found
            .iter()
            .find_map(|n| n.diff.as_ref().and_then(|d| d.old_ctx));
        assert_eq!(ctx, Some(131_072));
    }

    #[tokio::test]
    async fn registry_ctx_wins_over_the_server() {
        let server = llama_swap(
            serde_json::json!({"running": [{"model": "gemma", "state": "ready", "cmd": Q8_CMD}]}),
            "",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("models.txt");
        std::fs::write(&registry, "gemma|a|b|c|32768|d|--jinja\n").unwrap();
        let (_db, store) = sqlite();
        seed_slow_gemma(&store);
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
                ("LLM_HUB_LOCAL_PREFILL_GUARD", "60000"),
                ("LLM_HUB_LOCAL_TUNER_REGISTRY", registry.to_str().unwrap()),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            Some(store),
        );

        let found = run_once(&state, now_ms()).await;

        let old_ctx = found
            .iter()
            .find_map(|n| n.diff.as_ref().and_then(|d| d.old_ctx));
        assert_eq!(old_ctx, Some(32_768));
        assert!(!rules_of(&found).contains(&"kv_cache_q8".to_owned()));
    }

    #[tokio::test]
    async fn disabled_tuner_does_nothing() {
        let server = MockServer::start().await;
        let (_dir, store) = sqlite();
        seed_slow_gemma(&store);
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
            ],
            Some(store),
        );

        assert!(run_once(&state, now_ms()).await.is_empty());
        assert_eq!(state.local_tuner().fit("local/gemma"), None);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopped_managed_profile_keeps_its_fit_but_is_not_probed() {
        let server = MockServer::start().await;
        let (_dir, store) = sqlite();
        seed_slow_gemma(&store);
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
                ("LLM_HUB_LOCAL_COMMAND", "llama-swap --listen :0"),
                ("LLM_HUB_LOCAL_TIMEOUT_MS", "60000"),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            Some(store),
        );

        assert!(run_once(&state, now_ms()).await.is_empty());
        assert!(state.local_tuner().fit("local/gemma").is_some());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unreachable_server_and_no_store_raise_nothing() {
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            None,
        );

        assert!(run_once(&state, now_ms()).await.is_empty());
        assert_eq!(state.local_tuner().fit("local/gemma"), None);
    }

    #[tokio::test]
    async fn failed_props_and_logs_drop_only_that_evidence() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/running"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"running": [{"model": "gemma", "state": "ready", "cmd": GEMMA_CMD}]}),
            ))
            .mount(&server)
            .await;
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            None,
        );

        let found = run_once(&state, now_ms()).await;

        assert_eq!(rules_of(&found), ["kv_cache_q8"]);
    }

    #[tokio::test]
    async fn plain_server_without_props_raises_nothing() {
        let server = MockServer::start().await;
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", &server.uri()),
                ("LLM_HUB_LOCAL_TUNER", "true"),
            ],
            None,
        );

        assert!(run_once(&state, now_ms()).await.is_empty());
    }

    #[test]
    fn client_stats_group_by_signature_and_count_failures() {
        let mut rows = vec![
            row("gemma", 90_000, None, 200),
            row("gemma", 94_000, Some(300_000), 200),
            row("gemma", 98_000, None, 504),
            row("gemma", 1, None, 499),
        ];
        rows[0].tokens_out = 0;
        let mut bare = row("gemma", 20_000, Some(50_000), 200);
        bare.client = Some("pi/1.0".into());
        let mut anonymous = row("gemma", 5, Some(5), 200);
        anonymous.client = None;
        rows.push(bare);
        rows.push(anonymous);

        let stats = client_stats(rows.iter());

        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].client, "opencode/1.0");
        assert_eq!(stats[0].requests, 4);
        assert_eq!(stats[0].median_prompt_tokens, 94_000);
        assert_eq!(stats[0].median_ttft_ms, Some(300_000));
        assert!((stats[0].failed_or_timeout_share - 0.75).abs() < 1e-9);
        assert_eq!(stats[1].client, "pi/1.0");
        assert!(stats[1].failed_or_timeout_share.abs() < 1e-9);
    }

    #[test]
    fn budget_prefers_the_guard_then_first_byte_then_timeout() {
        let config = |vars: &[(&str, &str)]| {
            let mut all: HashMap<String, String> = [
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
            ]
            .iter()
            .chain(vars)
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
            all.insert("LLM_HUB_LOCAL_TUNER".into(), "true".into());
            HubConfig::from_map(&all).unwrap()
        };
        let budget = |vars: &[(&str, &str)]| budget_ms(config(vars).profile("local").unwrap());
        assert_eq!(budget(&[]), 0);
        assert_eq!(budget(&[("LLM_HUB_LOCAL_TIMEOUT_MS", "9000")]), 9_000);
        assert_eq!(
            budget(&[
                ("LLM_HUB_LOCAL_TIMEOUT_MS", "9000"),
                ("LLM_HUB_LOCAL_FIRST_BYTE_TIMEOUT_MS", "0")
            ]),
            9_000
        );
        assert_eq!(
            budget(&[
                ("LLM_HUB_LOCAL_TIMEOUT_MS", "9000"),
                ("LLM_HUB_LOCAL_FIRST_BYTE_TIMEOUT_MS", "4000")
            ]),
            4_000
        );
        assert_eq!(
            budget(&[
                ("LLM_HUB_LOCAL_FIRST_BYTE_TIMEOUT_MS", "4000"),
                ("LLM_HUB_LOCAL_PREFILL_GUARD", "1000")
            ]),
            1_000
        );
    }

    #[test]
    fn rule_flags_keep_only_what_the_rules_read() {
        let flags = rule_flags(&EffectiveFlags::parse(
            "llama-server --jinja -ctk q8_0 --cache-type-v=f16 --cache-reuse",
        ));
        assert_eq!(
            flags,
            HashMap::from([
                ("-ctk".to_owned(), Some("q8_0".to_owned())),
                ("--cache-type-v".to_owned(), Some("f16".to_owned())),
                ("--cache-reuse".to_owned(), None),
            ])
        );
    }

    #[test]
    fn median_takes_the_middle_value() {
        assert_eq!(median(Vec::new()), None);
        assert_eq!(median(vec![3, 1, 2]), Some(2));
        assert_eq!(median(vec![4, 1, 3, 2]), Some(3));
    }

    #[test]
    fn fits_skip_failed_unmeasured_and_fully_cached_rows() {
        let mut cached = row("gemma", 4_000, Some(10), 200);
        cached.cache_read_tokens = 4_000;
        let rows = [
            row("gemma", 4_000, Some(40_000), 500),
            row("gemma", 4_000, None, 200),
            cached,
        ];
        assert!(fit_models(rows.iter()).is_empty());
    }

    #[tokio::test]
    async fn loop_waits_for_its_first_delay() {
        let state = state(
            &[
                ("LLM_HUB_PROFILES", "local"),
                ("LLM_HUB_LOCAL_BASE_URL", "http://127.0.0.1:9"),
            ],
            None,
        );
        let handle = spawn_probe_loop(state);
        tokio::task::yield_now().await;
        assert!(!handle.is_finished());
        handle.abort();
    }
}

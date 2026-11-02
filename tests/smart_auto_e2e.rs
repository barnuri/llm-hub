//! Smart Auto end to end on the spawned binary: fallback policy, role switches, effort, the current
//! and history APIs, the store contents, history across a restart, and reviewer counts per tier.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[path = "support/hub.rs"]
mod hub;

use hub::{BodyHas, BodyLacks, HubProcess, completion, start_hub};

const HUB_CHAT_ROUTE: &str = "/v1/chat/completions";
const UPSTREAM_CHAT_ROUTE: &str = "/chat/completions";
const WORKER_HEADER: &str = "x-llm-hub-smart-auto-worker";
const EXCLUDE_HEADER: &str = "x-llm-hub-smart-auto-exclude";
const MIN_CONTEXT_HEADER: &str = "x-llm-hub-smart-auto-min-context";
const FALLBACKS_HEADER: &str = "x-llm-hub-fallbacks";
const EFFICIENCY: &str = "hub/smart-auto-efficiency";
const BALANCE: &str = "hub/smart-auto-balance";
const INTELLIGENCE: &str = "hub/smart-auto-intelligence";
/// Only a reviewer request carries this marker: its user text wraps the draft in `<draft>`.
const REVIEWER_MARKER: &str = "<draft>";
const ACCEPT_VERDICT: &str = r#"{"verdict":"accept"}"#;
const DRAFT: &str = "The sum is 5.";
const PLANNING_TEXT: &str = "plan the storage layer for the new service";
const SCOPED_TEXT: &str = "implement task 2 from agent-spec/storage/plan.md";
const DISPATCH_STATES: [&str; 3] = ["selected", "started", "completed"];
const HISTORY_LIMIT: usize = 100;
const DECISIONS_LIMIT: usize = 5;
const POLL_ATTEMPTS: usize = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// An upstream whose `/models` lists `entries`. It has no chat route until a test mounts one.
async fn listing_upstream(entries: Vec<Value>) -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": entries})),
        )
        .mount(&upstream)
        .await;
    upstream
}

/// Mounts a chat route that answers solver calls with `draft` and reviewer calls with an accepting verdict.
async fn mount_reviewed_chat(upstream: &MockServer, draft: &str) {
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .and(BodyLacks(REVIEWER_MARKER))
        .respond_with(completion(draft))
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .and(BodyHas(REVIEWER_MARKER))
        .respond_with(completion(ACCEPT_VERDICT))
        .mount(upstream)
        .await;
}

/// Two upstreams, `alpha` and `beta`, that list the given models and answer reviewed chat calls.
async fn reviewed_pair(alpha: Vec<Value>, beta: Vec<Value>) -> (MockServer, MockServer) {
    let alpha = listing_upstream(alpha).await;
    let beta = listing_upstream(beta).await;
    mount_reviewed_chat(&alpha, DRAFT).await;
    mount_reviewed_chat(&beta, DRAFT).await;
    (alpha, beta)
}

/// Starts the hub over `alpha` and `beta` with `extra` settings, once it lists `models` concrete models.
async fn start_pair(
    alpha: &MockServer,
    beta: &MockServer,
    models: usize,
    extra: &[(&str, String)],
) -> (HubProcess, String) {
    let mut vars: HashMap<String, String> = HashMap::from([
        ("LLM_HUB_PROFILES".to_owned(), "alpha,beta".to_owned()),
        ("LLM_HUB_ALPHA_BASE_URL".to_owned(), alpha.uri()),
        ("LLM_HUB_BETA_BASE_URL".to_owned(), beta.uri()),
    ]);
    vars.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone())),
    );
    let (hub, base) = start_hub(vars).await;
    wait_for_concrete_models(&base, models).await;
    (hub, base)
}

async fn wait_for_concrete_models(base: &str, expected: usize) {
    let client = reqwest::Client::new();
    for _ in 0..POLL_ATTEMPTS {
        let models: Value = client
            .get(format!("{base}/v1/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let listed = models["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|model| model["owned_by"] != "hub")
            .count();
        if listed == expected {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("the hub did not list {expected} concrete models");
}

fn chat_request(model: &str, messages: Vec<Value>) -> Value {
    json!({"model": model, "stream": false, "messages": messages})
}

fn user(text: &str) -> Value {
    json!({"role": "user", "content": text})
}

fn assistant(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

async fn post_chat(base: &str, headers: &[(&str, &str)], body: &Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}{HUB_CHAT_ROUTE}"))
        .json(body);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

/// The chat calls an upstream received: solver calls, or reviewer calls when `reviewer` is true.
async fn chat_calls(upstream: &MockServer, reviewer: bool) -> Vec<Request> {
    upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == UPSTREAM_CHAT_ROUTE)
        .filter(|request| {
            String::from_utf8_lossy(&request.body).contains(REVIEWER_MARKER) == reviewer
        })
        .collect()
}

async fn reviewer_calls(alpha: &MockServer, beta: &MockServer) -> usize {
    chat_calls(alpha, true).await.len() + chat_calls(beta, true).await.len()
}

async fn history(base: &str, query: &str) -> Vec<Value> {
    let body: Value = reqwest::Client::new()
        .get(format!("{base}/api/smart-auto/history?{query}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["events"].as_array().unwrap().clone()
}

fn states(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["state"].as_str().unwrap())
        .collect()
}

/// Waits until the history of `worker` holds `dispatches` completed dispatches, and returns it.
async fn wait_for_completed(base: &str, worker: &str, dispatches: usize) -> Vec<Value> {
    let query = format!("worker={worker}&limit={HISTORY_LIMIT}");
    for _ in 0..POLL_ATTEMPTS {
        let events = history(base, &query).await;
        let completed = events
            .iter()
            .filter(|event| event["state"] == "completed")
            .count();
        if completed >= dispatches {
            return events;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("worker {worker} never completed {dispatches} dispatches");
}

fn stored_events(store: &Path) -> i64 {
    Connection::open_with_flags(store, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|conn| {
            conn.query_row("SELECT COUNT(*) FROM selection_events", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .unwrap_or(0)
}

/// Waits until the store file holds at least `count` selection events, so the writer has flushed.
async fn wait_for_stored_events(store: &Path, count: i64) {
    for _ in 0..POLL_ATTEMPTS {
        if stored_events(store) >= count {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    panic!("the store holds fewer than {count} selection events");
}

/// Files directly in `dir` whose bytes contain `needle`.
fn files_holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|file| {
            file.is_file()
                && std::fs::read(file)
                    .unwrap()
                    .windows(needle.len())
                    .any(|window| window == needle)
        })
        .collect()
}

#[tokio::test]
async fn a_role_switch_moves_a_settled_pin_to_a_plan_backed_model() {
    let (alpha, beta) = reviewed_pair(
        vec![
            json!({"id": "claude-opus-4.6"}),
            json!({"id": "claude-haiku-4.5"}),
        ],
        vec![json!({"id": "claude-sonnet-4.6"})],
    )
    .await;
    let (_hub, base) = start_pair(&alpha, &beta, 3, &[]).await;
    const WORKER: &str = "run-switch/node-1";

    let planning = post_chat(
        &base,
        &[(WORKER_HEADER, WORKER)],
        &chat_request(BALANCE, vec![user(PLANNING_TEXT)]),
    )
    .await;
    assert_eq!(planning.status(), 200);
    assert_eq!(
        planning.headers()["x-llm-hub-model"],
        "alpha/claude-opus-4.6"
    );
    assert_eq!(planning.headers()["x-llm-hub-smart-auto-role"], "planning");
    assert_eq!(planning.headers()["x-llm-hub-smart-auto-reason"], "fresh");
    wait_for_completed(&base, WORKER, 1).await;

    let implementation = post_chat(
        &base,
        &[(WORKER_HEADER, WORKER)],
        &chat_request(
            BALANCE,
            vec![
                user(PLANNING_TEXT),
                assistant("Plan drafted."),
                user("Plan drafted, carry it out."),
                assistant("Working on it."),
                user(SCOPED_TEXT),
            ],
        ),
    )
    .await;
    assert_eq!(implementation.status(), 200);
    assert_eq!(
        implementation.headers()["x-llm-hub-model"],
        "alpha/claude-haiku-4.5"
    );
    assert_eq!(
        implementation.headers()["x-llm-hub-smart-auto-role"],
        "implementation"
    );
    assert_eq!(
        implementation.headers()["x-llm-hub-smart-auto-reason"],
        "role_switch"
    );
}

#[tokio::test]
async fn the_effort_in_the_body_is_one_the_routed_model_publishes() {
    let alpha = listing_upstream(vec![json!({
        "id": "m1",
        "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
    })])
    .await;
    let beta = listing_upstream(vec![json!({"id": "m2"})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .mount(&alpha)
        .await;
    let (_hub, base) = start_pair(&alpha, &beta, 2, &[]).await;

    let response = post_chat(
        &base,
        &[(EXCLUDE_HEADER, "beta/*")],
        &chat_request(EFFICIENCY, vec![user(PLANNING_TEXT)]),
    )
    .await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-model"], "alpha/m1");
    let effort = response.headers()["x-llm-hub-smart-auto-effort"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        ["low", "medium", "high"].contains(&effort.as_str()),
        "{effort}"
    );
    let sent = chat_calls(&alpha, false).await;
    assert_eq!(sent.len(), 1);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["reasoning_effort"], effort.as_str());
}

#[tokio::test]
async fn an_explicit_effort_the_model_publishes_is_sent_unchanged() {
    let alpha = listing_upstream(vec![json!({
        "id": "m1",
        "capabilities": {"supports": {"reasoning_effort": ["low", "medium", "high"]}}
    })])
    .await;
    let beta = listing_upstream(vec![json!({"id": "m2"})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .mount(&alpha)
        .await;
    let (_hub, base) = start_pair(&alpha, &beta, 2, &[]).await;
    let mut body = chat_request(EFFICIENCY, vec![user(PLANNING_TEXT)]);
    body["reasoning_effort"] = json!("low");

    let response = post_chat(&base, &[(EXCLUDE_HEADER, "beta/*")], &body).await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-llm-hub-smart-auto-effort"], "low");
    let sent = chat_calls(&alpha, false).await;
    assert_eq!(sent.len(), 1);
    let sent_body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(sent_body["reasoning_effort"], "low");
}

#[tokio::test]
async fn policy_holds_across_the_fallback_chain() {
    let alpha = listing_upstream(vec![json!({"id": "m1", "context_window": 128_000})]).await;
    let beta = listing_upstream(vec![json!({"id": "m2", "context_window": 1_000_000})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&beta)
        .await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .expect(0)
        .mount(&alpha)
        .await;
    let (_hub, base) = start_pair(&alpha, &beta, 2, &[]).await;

    let response = post_chat(
        &base,
        &[(MIN_CONTEXT_HEADER, "1m"), (FALLBACKS_HEADER, "alpha/m1")],
        &chat_request(EFFICIENCY, vec![user(PLANNING_TEXT)]),
    )
    .await;

    assert_eq!(response.status(), 429);
    assert_eq!(response.headers()["x-llm-hub-attempts"], "beta/m2=429");
    assert!(chat_calls(&alpha, false).await.is_empty());
}

#[tokio::test]
async fn an_empty_pool_makes_no_upstream_call() {
    let (alpha, beta) = reviewed_pair(vec![json!({"id": "m1"})], vec![json!({"id": "m2"})]).await;
    let (_hub, base) = start_pair(&alpha, &beta, 2, &[]).await;

    let response = post_chat(
        &base,
        &[(MIN_CONTEXT_HEADER, "10m")],
        &chat_request(BALANCE, vec![user(PLANNING_TEXT)]),
    )
    .await;

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("smart_auto_no_eligible_model"),
        "{message}"
    );
    assert!(message.contains("min context 10000000 tokens"), "{message}");
    assert!(chat_calls(&alpha, false).await.is_empty());
    assert!(chat_calls(&beta, false).await.is_empty());
}

#[tokio::test]
async fn review_sizes_the_reviewers_by_tier() {
    let (alpha, beta) = reviewed_pair(
        vec![
            json!({"id": "claude-opus-4.6"}),
            json!({"id": "claude-haiku-4.5"}),
        ],
        vec![json!({"id": "claude-sonnet-4.6"})],
    )
    .await;
    let (_hub, base) = start_pair(&alpha, &beta, 3, &[]).await;

    let mut reviewers = Vec::new();
    for (selector, worker) in [
        (EFFICIENCY, "run-review/efficiency"),
        (BALANCE, "run-review/balance"),
        (INTELLIGENCE, "run-review/intelligence"),
    ] {
        let before = reviewer_calls(&alpha, &beta).await;
        let response = post_chat(
            &base,
            &[(WORKER_HEADER, worker)],
            &chat_request(selector, vec![user(PLANNING_TEXT)]),
        )
        .await;
        assert_eq!(response.status(), 200, "{selector}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["choices"][0]["message"]["content"], DRAFT,
            "{selector}"
        );
        reviewers.push(reviewer_calls(&alpha, &beta).await - before);
    }

    assert_eq!(reviewers, [0, 1, 2]);
}

#[tokio::test]
async fn current_and_history_follow_each_dispatch() {
    let alpha = listing_upstream(vec![json!({"id": "m1"})]).await;
    let beta = listing_upstream(vec![json!({"id": "m2"})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .mount(&alpha)
        .await;
    let (_hub, base) = start_pair(&alpha, &beta, 2, &[]).await;
    const WORKER: &str = "run-current/node-1";
    let headers = [(EXCLUDE_HEADER, "beta/*"), (WORKER_HEADER, WORKER)];

    for dispatch in 1..=2 {
        let response = post_chat(
            &base,
            &headers,
            &chat_request(EFFICIENCY, vec![user(PLANNING_TEXT)]),
        )
        .await;
        assert_eq!(response.status(), 200);
        wait_for_completed(&base, WORKER, dispatch).await;
    }

    let current: Value = reqwest::Client::new()
        .get(format!("{base}/api/smart-auto/current?worker={WORKER}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current["selection"]["main"]["model"], "alpha/m1");
    assert_eq!(current["selection"]["state"], "completed");
    assert_eq!(current["selection"]["stale"], false);

    let events = history(&base, &format!("worker={WORKER}&limit={HISTORY_LIMIT}")).await;
    let expected: Vec<&str> = DISPATCH_STATES
        .iter()
        .chain(&DISPATCH_STATES)
        .copied()
        .collect();
    assert_eq!(states(&events), expected);
    let seqs: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    let later = history(
        &base,
        &format!(
            "worker={WORKER}&since_seq={}&limit={HISTORY_LIMIT}",
            seqs[2]
        ),
    )
    .await;
    assert_eq!(states(&later), DISPATCH_STATES);
    assert!(history(&base, "worker=run-other/node-9").await.is_empty());

    let decisions: Value = reqwest::Client::new()
        .get(format!(
            "{base}/api/smart-auto/decisions?limit={DECISIONS_LIMIT}"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let decisions = decisions["decisions"].as_array().unwrap();
    assert!(!decisions.is_empty());
    assert!(
        decisions
            .iter()
            .all(|decision| decision["model"] == "alpha/m1")
    );
}

#[tokio::test]
async fn the_store_keeps_no_prompt_text() {
    const PROMPT_MARKER: &str = "e2e-prompt-marker-7c2d";
    const WORKER: &str = "run-store/node-1";
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.db");
    let alpha = listing_upstream(vec![json!({"id": "m1"})]).await;
    let beta = listing_upstream(vec![json!({"id": "m2"})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .mount(&alpha)
        .await;
    let extra = [
        ("LLM_HUB_PERSISTENT", "true".to_owned()),
        ("LLM_HUB_STORE_PATH", store.display().to_string()),
    ];
    let (_hub, base) = start_pair(&alpha, &beta, 2, &extra).await;
    let prompt = format!("{PLANNING_TEXT} {PROMPT_MARKER}");

    let response = post_chat(
        &base,
        &[(EXCLUDE_HEADER, "beta/*"), (WORKER_HEADER, WORKER)],
        &chat_request(EFFICIENCY, vec![user(&prompt)]),
    )
    .await;

    assert_eq!(response.status(), 200);
    let events = wait_for_completed(&base, WORKER, 1).await;
    wait_for_stored_events(&store, DISPATCH_STATES.len() as i64).await;
    assert!(
        files_holding(dir.path(), PROMPT_MARKER.as_bytes()).is_empty(),
        "the store holds prompt text"
    );
    assert!(!Value::Array(events).to_string().contains(PROMPT_MARKER));
}

#[tokio::test]
async fn history_survives_a_restart() {
    const WORKER: &str = "run-restart/node-1";
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.db");
    let extra = [
        ("LLM_HUB_PERSISTENT", "true".to_owned()),
        ("LLM_HUB_STORE_PATH", store.display().to_string()),
    ];
    let alpha = listing_upstream(vec![json!({"id": "m1"})]).await;
    let beta = listing_upstream(vec![json!({"id": "m2"})]).await;
    Mock::given(method("POST"))
        .and(path(UPSTREAM_CHAT_ROUTE))
        .respond_with(completion(DRAFT))
        .mount(&alpha)
        .await;
    let query = format!("worker={WORKER}&limit={HISTORY_LIMIT}");

    let (hub, base) = start_pair(&alpha, &beta, 2, &extra).await;
    let response = post_chat(
        &base,
        &[(EXCLUDE_HEADER, "beta/*"), (WORKER_HEADER, WORKER)],
        &chat_request(EFFICIENCY, vec![user(PLANNING_TEXT)]),
    )
    .await;
    assert_eq!(response.status(), 200);
    wait_for_completed(&base, WORKER, 1).await;
    wait_for_stored_events(&store, DISPATCH_STATES.len() as i64).await;
    drop(hub);

    let (_restarted, base) = start_pair(&alpha, &beta, 2, &extra).await;
    let events = history(&base, &query).await;
    assert_eq!(states(&events), DISPATCH_STATES);
}

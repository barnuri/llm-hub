//! Partial aggregation: live upstreams merge, a dead upstream degrades to
//! its static list — never a 502.

use std::collections::HashMap;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[path = "support/hub.rs"]
mod hub;

use hub::{free_port, start_hub};

fn models_response(ids: &[&str]) -> ResponseTemplate {
    let data: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            let mut model =
                serde_json::json!({"id": id, "object": "model", "owned_by": "upstream"});
            if *id == "bedrock/anthropic.claude-opus-5" {
                model["max_input_tokens"] = serde_json::json!(1_000_000);
            }
            model
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(serde_json::json!({"object": "list", "data": data}))
}

#[tokio::test]
async fn aggregates_partially_when_one_upstream_is_down() {
    let alive_a = MockServer::start().await;
    let alive_b = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(models_response(&[
            "gpt-4o",
            "bedrock/anthropic.claude-opus-5",
        ]))
        .mount(&alive_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(models_response(&["meta-llama/Llama-4-Scout"]))
        .mount(&alive_b)
        .await;

    let dead_port = free_port();
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "a,b,dead,disabled".into()),
        ("LLM_HUB_A_BASE_URL".into(), alive_a.uri()),
        ("LLM_HUB_B_BASE_URL".into(), alive_b.uri()),
        (
            "LLM_HUB_DEAD_BASE_URL".into(),
            format!("http://127.0.0.1:{dead_port}"),
        ),
        ("LLM_HUB_DEAD_MODELS".into(), "static-x".into()),
        ("LLM_HUB_DISABLED_BASE_URL".into(), alive_b.uri()),
        ("LLM_HUB_DISABLED_ENABLED".into(), "false".into()),
        ("LLM_HUB_DISABLED_MODELS".into(), "hidden".into()),
    ]))
    .await;

    let response = reqwest::get(format!("{base}/v1/models")).await.unwrap();
    assert_eq!(
        response.status(),
        200,
        "dead upstream must never cause a 502"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();

    assert!(ids.contains(&"a/gpt-4o"));
    assert!(
        ids.contains(&"a/bedrock/anthropic.claude-opus-5[1m]"),
        "1M Claude families are advertised with [1m] so Claude Code assumes 1M"
    );
    let opus = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "a/bedrock/anthropic.claude-opus-5[1m]")
        .unwrap();
    assert_eq!(opus["max_input_tokens"], 1_000_000);
    assert!(
        ids.contains(&"b/meta-llama/Llama-4-Scout"),
        "slash-containing upstream id keeps full path"
    );
    assert!(
        ids.contains(&"dead/static-x"),
        "dead upstream degrades to its static list"
    );
    assert!(
        !ids.contains(&"disabled/hidden"),
        "disabled profiles must not appear in the models API"
    );
    drop(hub);
}

async fn copilot_upstream(models: serde_json::Value) -> (MockServer, MockServer) {
    let github = MockServer::start().await;
    let copilot = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "chat_enabled": true,
            "endpoints": {"api": copilot.uri()},
        })))
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": models})))
        .mount(&copilot)
        .await;
    (github, copilot)
}

#[tokio::test]
async fn copilot_entries_add_output_and_limit_provenance_beside_the_old_fields() {
    let (github, _copilot) = copilot_upstream(serde_json::json!([
        {"id": "claude-opus-5.5", "supported_endpoints": ["/v1/messages"],
         "capabilities": {"limits": {"max_context_window_tokens": 328_000,
                                     "max_output_tokens": 128_000,
                                     "max_prompt_tokens": 200_000}}},
        {"id": "gpt-new", "supported_endpoints": ["/responses"],
         "capabilities": {"limits": {"max_prompt_tokens": 272_000}}}
    ]))
    .await;
    let dead_port = free_port();
    let (hub, base) = start_hub(HashMap::from([
        ("LLM_HUB_PROFILES".into(), "copilot".into()),
        (
            "LLM_HUB_COPILOT_BASE_URL".into(),
            format!("http://127.0.0.1:{dead_port}"),
        ),
        ("LLM_HUB_COPILOT_AUTH".into(), "github-copilot".into()),
        ("LLM_HUB_COPILOT_GITHUB_TOKEN_SOURCE".into(), "gh".into()),
        ("LLM_HUB_COPILOT_GITHUB_API_URL".into(), github.uri()),
        ("GH_TOKEN".into(), "gho_test".into()),
    ]))
    .await;

    let body: serde_json::Value = reqwest::get(format!("{base}/v1/models"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = |id: &str| {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from {body}"))
            .clone()
    };

    let opus = entry("copilot/claude-opus-5-5[1m]");
    for field in ["max_input_tokens", "context_length", "context_window"] {
        assert_eq!(opus[field], 1_000_000, "{field}");
    }
    assert_eq!(opus["max_output_tokens"], 128_000);
    let limits = &opus["llm_hub_limits"];
    assert_eq!(limits["input"]["value"], 1_000_000);
    assert_eq!(limits["input"]["source"], "seeded");
    assert_eq!(limits["context"]["value"], 328_000);
    assert_eq!(limits["output"]["value"], 128_000);
    assert_eq!(limits["output"]["source"], "listed");
    assert_eq!(limits["output"]["conflict"], false);
    assert!(limits.get("runtime_context").is_none(), "{opus}");
    assert!(limits["output"].get("observed_at").is_none(), "{opus}");

    let gpt = entry("copilot/gpt-new");
    for field in ["max_input_tokens", "context_length", "context_window"] {
        assert_eq!(gpt[field], 272_000, "{field}");
    }
    assert!(gpt.get("max_output_tokens").is_none(), "{gpt}");
    assert!(gpt["llm_hub_limits"].get("output").is_none(), "{gpt}");
    assert_eq!(gpt["llm_hub_limits"]["input"]["source"], "listed");

    let balance = entry("hub/smart-auto-balance");
    assert_eq!(balance["owned_by"], "hub");
    assert_eq!(balance["recommended"], true);
    assert!(balance["max_input_tokens"].is_u64(), "{balance}");
    assert!(balance["context_length"].is_u64(), "{balance}");
    assert_eq!(entry("hub/smart-auto-efficiency")["recommended"], false);
    drop(hub);
}

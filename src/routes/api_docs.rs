//! `OpenAPI` document. Paths come from `#[utoipa::path]` on each handler and are collected by
//! the `OpenApiRouter` in `build_router`. This module holds the document header, the security
//! schemes, and the doc-only entries for the pass-through `/v1/{path}` catch-all.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::schemas::error_body::{ErrorBody, ErrorDetail};

const PROXY_DESCRIPTION: &str = "Pass-through, not a typed API. The hub forwards any `/v1/*` path that has no dedicated route to the upstream profile its target model names, and returns the upstream response unchanged (status, headers, body, SSE stream). \
The target comes from the `model` field of a JSON body (or a multipart `model` part), written `<profile>/<model>`. The hub strips the profile prefix before it forwards. \
Known examples: `POST /v1/chat/completions`, `POST /v1/responses`, `POST /v1/embeddings`, `POST /v1/audio/transcriptions`, `POST /v1/messages/count_tokens`. \
Request and response shapes are those of the upstream provider, so this document does not describe them. \
Optional `X-LLM-Hub-*` request headers set fallbacks, timeout, retry statuses and body transforms. `X-LLM-Hub-Model` and `X-LLM-Hub-Attempts` response headers report what served the request. \
The hub answers 400 when the model has no known profile prefix, 401 on a bad hub key, and 502 when every attempt fails.";

/// Documents one HTTP method of the catch-all. Handler: `routes::proxy::proxy`.
macro_rules! proxy_doc {
    ($name:ident, $method:ident, $summary:literal) => {
        #[utoipa::path($method, path = "/v1/{path}", tag = "proxy", summary = $summary,
            description = PROXY_DESCRIPTION,
            params(("path" = String, Path, description = "Rest of the upstream path, for example `chat/completions`. May contain slashes.")),
            responses(
                (status = 200, description = "The upstream response, passed through unchanged. Streams as SSE when the upstream streams.", content((Object = "application/json"), (String = "text/event-stream"))),
                (status = 400, description = "Model missing or without a known profile prefix, or body too large.", body = ErrorBody),
                (status = 401, description = "Missing or invalid hub key.", body = ErrorBody),
                (status = 502, description = "Every attempt failed.", body = ErrorBody)
            ))]
        #[allow(dead_code)]
        pub fn $name() {}
    };
}

proxy_doc!(proxy_get, get, "Pass-through GET to the upstream");
proxy_doc!(proxy_post, post, "Pass-through POST to the upstream");
proxy_doc!(proxy_put, put, "Pass-through PUT to the upstream");
proxy_doc!(proxy_patch, patch, "Pass-through PATCH to the upstream");
proxy_doc!(proxy_delete, delete, "Pass-through DELETE to the upstream");

struct HubSecurity;

impl Modify for HubSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Hub master key or an enabled hub API key, sent as `Authorization: Bearer <key>`. Needed only when hub auth is on.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "api_key",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-api-key",
                "Same key as `bearer`, sent as the `x-api-key` header. Needed only when hub auth is on.",
            ))),
        );
        // Either scheme is accepted. With hub auth off, the hub accepts any or no key.
        openapi.security = Some(vec![
            utoipa::openapi::security::SecurityRequirement::new("bearer", Vec::<String>::new()),
            utoipa::openapi::security::SecurityRequirement::new("api_key", Vec::<String>::new()),
        ]);
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "llm-hub",
        description = "Single-binary LLM proxy. One port speaks OpenAI chat completions, OpenAI Responses and Anthropic Messages, and routes by `<profile>/<model>`. \
The `/api/*` routes feed the dashboard. The dashboard itself (any other GET path) is a web UI and is not part of this document.\n\n\
Auth: when `LLM_HUB_MASTER_KEY` is set or a hub API key exists, every `/v1/*`, `/api/*`, `/openapi.json` and `/api-docs` request needs a key. With neither, auth is off and any or no key is accepted. `/healthz` never needs a key.",
        license(name = "MIT", identifier = "MIT"),
    ),
    paths(crate::routes::admin::healthz, crate::routes::proxy::get_model_by_id, proxy_get, proxy_post, proxy_put, proxy_patch, proxy_delete),
    components(schemas(ErrorBody, ErrorDetail, crate::schemas::insights::pick_by::PickBy)),
    modifiers(&HubSecurity),
    tags(
        (name = "proxy", description = "Model traffic: Anthropic Messages and the pass-through catch-all."),
        (name = "models", description = "Model catalog."),
        (name = "profiles", description = "Upstream profiles, fallbacks, keep-warm and quotas."),
        (name = "keys", description = "Hub API keys."),
        (name = "stats", description = "Request stats, usage, errors and the live feed."),
        (name = "insights", description = "Model rankings."),
        (name = "sessions", description = "Coding-agent sessions."),
        (name = "smart-auto", description = "Smart Auto routing: selections, rules, outcomes, dry runs."),
        (name = "copilot", description = "GitHub Copilot quota, prices and Auto decisions."),
        (name = "system", description = "Liveness and restart."),
    )
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::Value;

    use crate::configs::HubConfig;
    use crate::dependencies::state::AppState;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const SPEC_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.json");
    const MAIN_SOURCE: &str = include_str!("../main.rs");

    fn generate() -> (String, Value) {
        let vars: HashMap<String, String> = [
            ("LLM_HUB_PROFILES", "copilot"),
            ("LLM_HUB_COPILOT_BASE_URL", "http://127.0.0.1:9"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let state = AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap();
        let openapi = crate::api_router(&state).into_openapi();
        let mut text = openapi.to_pretty_json().unwrap();
        text.push('\n');
        let value = serde_json::from_str(&text).unwrap();
        (text, value)
    }

    /// Run `UPDATE_OPENAPI=1 cargo test openapi_file` to rewrite docs/openapi.json.
    /// Without it, the test fails when the committed file differs from the code.
    #[test]
    fn openapi_file_matches_the_code() {
        let (generated, _) = generate();
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::write(SPEC_FILE, &generated).unwrap();
            return;
        }
        // A Windows checkout may turn the committed file's line endings into CRLF.
        let committed = std::fs::read_to_string(SPEC_FILE)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            committed == generated,
            "docs/openapi.json is out of date. Regenerate: UPDATE_OPENAPI=1 cargo test openapi_file"
        );
    }

    #[test]
    fn operation_ids_are_unique() {
        let (_, spec) = generate();
        let mut seen = std::collections::HashMap::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                let Some(id) = op["operationId"].as_str() else {
                    continue;
                };
                if let Some(prev) = seen.insert(id.to_string(), format!("{method} {path}")) {
                    panic!("operationId {id} used by {prev} and {method} {path}");
                }
            }
        }
    }

    /// Runs the offline validator when `bunx` is on PATH. Skips otherwise.
    #[test]
    fn committed_spec_passes_openapi_parser_if_available() {
        let Ok(bunx) = which_bunx() else { return };
        let out = std::process::Command::new(bunx)
            .args(["--bun", "@readme/openapi-parser", "validate", SPEC_FILE])
            .output();
        let Ok(out) = out else { return };
        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        // No network or registry: skip rather than fail.
        if !out.status.success()
            && (text.contains("downloading package manifest") || text.contains("registry"))
        {
            return;
        }
        assert!(out.status.success(), "openapi-parser failed: {text}");
    }

    fn which_bunx() -> Result<std::path::PathBuf, ()> {
        std::env::var_os("PATH")
            .and_then(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join("bunx"))
                    .find(|c| c.is_file())
            })
            .ok_or(())
    }

    #[test]
    fn document_is_openapi_3_1_with_both_key_schemes() {
        let (_, spec) = generate();
        assert!(spec["openapi"].as_str().unwrap().starts_with("3.1"));
        let schemes = &spec["components"]["securitySchemes"];
        assert_eq!(schemes["bearer"]["scheme"], "bearer");
        assert_eq!(schemes["api_key"]["name"], "x-api-key");
        assert_eq!(schemes["api_key"]["in"], "header");
    }

    /// Typed `routes!` registration already forces `#[utoipa::path]`. This covers the raw
    /// `.route("...")` calls that axum needs for wildcards: each must have a documented path.
    #[test]
    fn every_raw_route_has_a_documented_path() {
        let (_, spec) = generate();
        let paths = spec["paths"].as_object().unwrap();
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        let mut seen = 0;
        for line in production.lines() {
            let Some(rest) = line.trim().strip_prefix(".route(\"") else {
                continue;
            };
            let raw = rest.split('"').next().unwrap();
            let documented = raw.replace("{*", "{");
            assert!(
                paths.contains_key(&documented),
                "route {raw} has no #[utoipa::path] ({documented} missing from the document)"
            );
            seen += 1;
        }
        assert!(seen >= 2, "expected the wildcard routes, saw {seen}");
    }

    #[test]
    fn healthz_is_documented_without_a_key() {
        let (_, spec) = generate();
        assert_eq!(
            spec["paths"]["/healthz"]["get"]["security"],
            serde_json::json!([{}])
        );
    }

    #[test]
    fn every_operation_is_described() {
        let (_, spec) = generate();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                assert!(op["tags"].is_array(), "{method} {path} has no tag");
                assert!(op["summary"].is_string(), "{method} {path} has no summary");
                assert!(
                    op["responses"]["200"].is_object(),
                    "{method} {path} has no 200"
                );
            }
        }
    }
}

use std::collections::HashMap;

use serde_json::Value;

use crate::configs::ProfileConfig;
use crate::routes::models::advertised_bare_id;
use crate::schemas::evidence_source::EvidenceSource;
use crate::schemas::limit_evidence::LimitEvidence;
use crate::schemas::model_context::{
    LISTED_CONFIDENCE, LOCAL_RUNTIME_CONFIDENCE, input_limit_evidence_from_item,
    llama_swap_context_tokens,
};
use crate::schemas::model_descriptor::ModelDescriptor;
use crate::services::copilot_auto::{CHAT_ENDPOINT, MESSAGES_ENDPOINT, RESPONSES_ENDPOINT};
use crate::services::model_limits::{LimitSource, ModelLimit};

/// Turns `/models` catalog entries plus a profile's known prompt limits into
/// [`ModelDescriptor`]s. Pure: no network, disk or clock access.
#[derive(Debug)]
pub struct ModelDescriptors;

impl ModelDescriptors {
    const CLAUDE_ID_PREFIX: &str = "claude-";
    const CHAT_ROUTE: &str = "/v1/chat/completions";
    const RESPONSES_ROUTE: &str = "/v1/responses";
    const CHAT_CAPABILITY_TYPE: &str = "chat";
    /// Id fragments of speech-to-text and text-to-speech models, which list no capability type.
    const SPEECH_ID_MARKERS: [&str; 4] = ["whisper", "tts", "transcribe", "speech"];
    const LEARNED_CONFIDENCE: f32 = 0.95;
    const SEEDED_CONFIDENCE: f32 = 0.85;

    /// One descriptor per entry. `limits` holds the profile's learned and seeded prompt limits.
    #[must_use]
    pub fn build(
        profile: &ProfileConfig,
        items: &[&Value],
        limits: Option<&HashMap<String, ModelLimit>>,
    ) -> Vec<ModelDescriptor> {
        items
            .iter()
            .filter_map(|item| Self::describe(profile, item, limits))
            .collect()
    }

    /// `None` when the entry has no id.
    #[must_use]
    pub fn describe(
        profile: &ProfileConfig,
        item: &Value,
        limits: Option<&HashMap<String, ModelLimit>>,
    ) -> Option<ModelDescriptor> {
        let raw_id = item.get("id").and_then(Value::as_str)?;
        let endpoints = Self::client_routes(item);
        let scope = Self::scope(&profile.name, &endpoints);
        let runtime_context = llama_swap_context_tokens(item).map(|value| {
            LimitEvidence::new(
                value,
                EvidenceSource::LocalRuntime,
                &scope,
                LOCAL_RUNTIME_CONFIDENCE,
            )
        });
        let context = Self::listed_u64(item, "/capabilities/limits/max_context_window_tokens")
            .or_else(|| Self::listed_u64(item, "/context_window"))
            .or_else(|| Self::listed_u64(item, "/context_length"))
            .map(|value| {
                LimitEvidence::new(value, EvidenceSource::Listed, &scope, LISTED_CONFIDENCE)
            })
            .or_else(|| runtime_context.clone());
        let output =
            Self::listed_u64(item, "/capabilities/limits/max_output_tokens").map(|value| {
                LimitEvidence::new(value, EvidenceSource::Listed, &scope, LISTED_CONFIDENCE)
            });
        Some(ModelDescriptor {
            qualified_id: format!("{}/{}", profile.name, advertised_bare_id(profile, raw_id)),
            profile: profile.name.clone(),
            input: Self::input(profile, raw_id, item, &endpoints, &scope, limits),
            endpoints,
            context,
            output,
            runtime_context,
            effort_levels: Self::effort_levels(item),
            chat: Self::is_chat(raw_id, item),
            vision: item
                .pointer("/capabilities/supports/vision")
                .and_then(Value::as_bool),
            tool_support: item
                .pointer("/capabilities/supports/tool_calls")
                .and_then(Value::as_bool),
        })
    }

    /// Learned rejection, then the measured seed, then the listed `/models` value. Copilot
    /// accepts more than it lists for Claude on `/v1/messages`, so that listing never counts.
    fn input(
        profile: &ProfileConfig,
        raw_id: &str,
        item: &Value,
        endpoints: &[String],
        scope: &str,
        limits: Option<&HashMap<String, ModelLimit>>,
    ) -> Option<LimitEvidence> {
        let listed_is_floor = profile.copilot.is_some()
            && raw_id.starts_with(Self::CLAUDE_ID_PREFIX)
            && endpoints.iter().any(|route| route == MESSAGES_ENDPOINT);
        let listed = input_limit_evidence_from_item(item, scope)
            .filter(|evidence| !(listed_is_floor && evidence.source == EvidenceSource::Listed));
        let Some(known) = limits.and_then(|limits| limits.get(raw_id)) else {
            return listed;
        };
        let source = EvidenceSource::from(known.source);
        let mut evidence = match known.source {
            LimitSource::Learned => LimitEvidence {
                observed_at: Some(known.learned_at),
                ..LimitEvidence::new(known.limit, source, scope, Self::LEARNED_CONFIDENCE)
            },
            LimitSource::Seeded => {
                LimitEvidence::new(known.limit, source, scope, Self::SEEDED_CONFIDENCE)
            }
        };
        evidence.conflict = listed.is_some_and(|listed| evidence.conflicts_with(&listed));
        Some(evidence)
    }

    /// The hub routes that reach this model, from Copilot `supported_endpoints`. The proxy
    /// bridges `/v1/messages` to a Responses model. Empty when the upstream lists none.
    fn client_routes(item: &Value) -> Vec<String> {
        let listed: Vec<&str> = item
            .get("supported_endpoints")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let serves = |endpoint: &str| listed.contains(&endpoint);
        let mut routes = Vec::new();
        if serves(MESSAGES_ENDPOINT) || serves(RESPONSES_ENDPOINT) {
            routes.push(MESSAGES_ENDPOINT.to_string());
        }
        if serves(CHAT_ENDPOINT) {
            routes.push(Self::CHAT_ROUTE.to_string());
        }
        if serves(RESPONSES_ENDPOINT) {
            routes.push(Self::RESPONSES_ROUTE.to_string());
        }
        routes
    }

    fn scope(profile: &str, endpoints: &[String]) -> String {
        match endpoints.first() {
            Some(route) => format!("{profile}:{route}"),
            None => profile.to_string(),
        }
    }

    /// The listed capability type decides. With none listed, a speech id marker means not chat.
    fn is_chat(raw_id: &str, item: &Value) -> bool {
        if let Some(kind) = item.pointer("/capabilities/type").and_then(Value::as_str) {
            return kind == Self::CHAT_CAPABILITY_TYPE;
        }
        let id = raw_id.to_ascii_lowercase();
        !Self::SPEECH_ID_MARKERS
            .iter()
            .any(|marker| id.contains(marker))
    }

    /// Only the levels the upstream publishes, in its order.
    fn effort_levels(item: &Value) -> Vec<String> {
        item.pointer("/capabilities/supports/reasoning_effort")
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn listed_u64(item: &Value, pointer: &str) -> Option<u64> {
        item.pointer(pointer)
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::configs::{CopilotAuth, TokenRates};
    use crate::consts::CONTEXT_TOKENS_1M;
    use crate::services::model_limits::ModelLimits;

    fn profile(name: &str, copilot: bool) -> ProfileConfig {
        let copilot_env = |key: &str| (key == "AUTH").then(|| "github-copilot".to_string());
        let copilot = copilot.then(|| CopilotAuth::from_env("T", &copilot_env).unwrap().unwrap());
        ProfileConfig {
            name: name.to_string(),
            display_name: None,
            base_url: String::new(),
            api_key: String::new(),
            extra_headers: Vec::new(),
            timeout_ms: None,
            enabled: true,
            static_models: Vec::new(),
            pricing: TokenRates::default(),
            model_prices: HashMap::new(),
            managed: None,
            copilot,
        }
    }

    fn copilot_claude() -> Value {
        json!({
            "id": "claude-opus-5.5",
            "supported_endpoints": ["/v1/messages", "/chat/completions"],
            "capabilities": {
                "type": "chat",
                "limits": {
                    "max_context_window_tokens": 328_000,
                    "max_output_tokens": 128_000,
                    "max_prompt_tokens": 200_000
                },
                "supports": {
                    "reasoning_effort": ["low", "medium", "high", "xhigh", "max"],
                    "tool_calls": true,
                    "vision": true
                }
            }
        })
    }

    fn copilot_gpt(id: &str) -> Value {
        json!({
            "id": id,
            "supported_endpoints": ["/responses", "ws:/responses"],
            "capabilities": {
                "limits": { "max_prompt_tokens": 272_000 },
                "supports": { "reasoning_effort": ["none", "low", "medium", "high"] }
            }
        })
    }

    fn learned(limit: u64) -> ModelLimit {
        ModelLimit {
            limit,
            source: LimitSource::Learned,
            learned_at: 42,
        }
    }

    #[test]
    fn copilot_entry_reads_listed_context_output_effort_and_seeded_input() {
        let limits = ModelLimits::default().for_profile("copilot");
        let descriptor =
            ModelDescriptors::describe(&profile("copilot", true), &copilot_claude(), Some(&limits))
                .unwrap();
        assert_eq!(descriptor.qualified_id, "copilot/claude-opus-5-5");
        assert_eq!(
            descriptor.endpoints,
            ["/v1/messages", "/v1/chat/completions"]
        );
        let input = descriptor.input.unwrap();
        assert_eq!(input.value, 1_000_000);
        assert_eq!(input.source, EvidenceSource::Seeded);
        assert_eq!(input.scope, "copilot:/v1/messages");
        assert!(!input.conflict);
        assert_eq!(descriptor.context.unwrap().value, 328_000);
        let output = descriptor.output.unwrap();
        assert_eq!(output.value, 128_000);
        assert_eq!(output.source, EvidenceSource::Listed);
        assert_eq!(
            descriptor.effort_levels,
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert!(descriptor.chat);
        assert_eq!(descriptor.vision, Some(true));
        assert_eq!(descriptor.tool_support, Some(true));
        assert_eq!(descriptor.runtime_context, None);
    }

    #[test]
    fn learned_limit_wins_over_seed_and_listing() {
        let mut limits = (*ModelLimits::default().for_profile("copilot")).clone();
        limits.insert("gpt-6.1-sol".to_string(), learned(900_000));
        let descriptor = ModelDescriptors::describe(
            &profile("copilot", true),
            &copilot_gpt("gpt-6.1-sol"),
            Some(&limits),
        )
        .unwrap();
        let input = descriptor.input.unwrap();
        assert_eq!(input.value, 900_000);
        assert_eq!(input.source, EvidenceSource::ObservedRejection);
        assert_eq!(input.observed_at, Some(42));
        assert!(input.conflict);
    }

    #[test]
    fn seed_wins_over_listing_and_flags_the_disagreement() {
        let limits = ModelLimits::default().for_profile("copilot");
        let descriptor = ModelDescriptors::describe(
            &profile("copilot", true),
            &copilot_gpt("gpt-6.1-sol"),
            Some(&limits),
        )
        .unwrap();
        let input = descriptor.input.unwrap();
        assert_eq!(input.value, 922_000);
        assert_eq!(input.source, EvidenceSource::Seeded);
        assert!(input.conflict);
        assert_eq!(input.scope, "copilot:/v1/messages");
        assert_eq!(
            descriptor.endpoints,
            ["/v1/messages", "/v1/responses"],
            "ws:/responses is not a hub route"
        );
        assert_eq!(descriptor.effort_levels, ["none", "low", "medium", "high"]);
    }

    #[test]
    fn unseeded_model_falls_back_to_listing() {
        let descriptor = ModelDescriptors::describe(
            &profile("copilot", true),
            &copilot_gpt("gpt-new"),
            Some(&HashMap::new()),
        )
        .unwrap();
        let input = descriptor.input.unwrap();
        assert_eq!(input.value, 272_000);
        assert_eq!(input.source, EvidenceSource::Listed);
        assert_eq!(descriptor.output, None, "unknown output stays unknown");
    }

    #[test]
    fn unseeded_copilot_claude_on_messages_listing_is_a_floor_so_input_is_unknown() {
        let mut item = copilot_claude();
        item["id"] = json!("claude-new");
        let descriptor =
            ModelDescriptors::describe(&profile("copilot", true), &item, Some(&HashMap::new()))
                .unwrap();
        assert_eq!(descriptor.input, None);
    }

    #[test]
    fn llama_swap_entry_runtime_context_is_local_runtime_evidence() {
        let item = json!({
            "id": "qwen3.6-35b",
            "meta": { "llamaswap": { "context_length": 262_144 } }
        });
        let descriptor =
            ModelDescriptors::describe(&profile("llama_swap", false), &item, None).unwrap();
        let runtime = descriptor.runtime_context.unwrap();
        assert_eq!(runtime.value, 262_144);
        assert_eq!(runtime.source, EvidenceSource::LocalRuntime);
        assert_eq!(runtime.scope, "llama_swap");
        assert_eq!(descriptor.context, Some(runtime.clone()));
        assert_eq!(
            descriptor.input.unwrap().source,
            EvidenceSource::LocalRuntime
        );
        assert_eq!(descriptor.output, None);
        assert!(descriptor.effort_levels.is_empty());
        assert!(descriptor.endpoints.is_empty());
    }

    #[test]
    fn generic_listed_entry_top_level_context_is_listed() {
        let item = json!({ "id": "anthropic.claude-opus-5", "context_length": 1_000_000 });
        let descriptor = ModelDescriptors::describe(&profile("llmgw", false), &item, None).unwrap();
        assert_eq!(descriptor.qualified_id, "llmgw/anthropic.claude-opus-5");
        let input = descriptor.input.unwrap();
        assert_eq!(input.source, EvidenceSource::Listed);
        assert_eq!(input.value, 1_000_000);
        assert_eq!(descriptor.context.unwrap().value, 1_000_000);
        assert_eq!(descriptor.vision, None);
        assert_eq!(descriptor.tool_support, None);
    }

    #[test]
    fn id_only_entry_every_limit_is_unknown() {
        let item = json!({ "id": "static-model" });
        let descriptor =
            ModelDescriptors::describe(&profile("static", false), &item, None).unwrap();
        assert_eq!(descriptor.input, None);
        assert_eq!(descriptor.context, None);
        assert_eq!(descriptor.output, None);
        assert_eq!(descriptor.runtime_context, None);
        assert!(descriptor.chat);
    }

    #[test]
    fn entry_without_id_is_skipped() {
        let items = [&json!({ "object": "model" }), &json!({ "id": "kept" })];
        let descriptors = ModelDescriptors::build(&profile("local", false), &items, None);
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].qualified_id, "local/kept");
    }

    #[test]
    fn non_chat_capability_is_not_chat() {
        let item = json!({ "id": "tts-1", "capabilities": { "type": "speech" } });
        let descriptor =
            ModelDescriptors::describe(&profile("copilot", true), &item, None).unwrap();
        assert!(!descriptor.chat);
    }

    #[test]
    fn speech_model_without_capability_type_is_not_chat() {
        for id in [
            "whisper",
            "ivrit-whisper-large",
            "kokoro-tts",
            "gpt-4o-transcribe",
        ] {
            let item = json!({ "id": id });
            let descriptor =
                ModelDescriptors::describe(&profile("local", false), &item, None).unwrap();
            assert!(!descriptor.chat, "{id}");
        }
        let item = json!({ "id": "qwen3.6-35b" });
        let descriptor = ModelDescriptors::describe(&profile("local", false), &item, None).unwrap();
        assert!(descriptor.chat);
    }

    #[test]
    fn one_million_boundary_seeds_split_at_exactly_one_million() {
        let limits = ModelLimits::default().for_profile("copilot");
        let terra = ModelDescriptors::describe(
            &profile("copilot", true),
            &copilot_gpt("gpt-6.1-terra"),
            Some(&limits),
        )
        .unwrap();
        let sol = ModelDescriptors::describe(
            &profile("copilot", true),
            &copilot_gpt("gpt-6.1-sol"),
            Some(&limits),
        )
        .unwrap();
        assert!(terra.input.unwrap().value >= CONTEXT_TOKENS_1M);
        assert!(sol.input.unwrap().value < CONTEXT_TOKENS_1M);
    }
}

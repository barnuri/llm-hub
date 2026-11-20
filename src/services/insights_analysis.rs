//! The manual AI analysis of the insights report: its compact input, the prompt, the reply
//! text, and the models to recommend for it. Nothing here runs unless a user asks for a run.

use std::cmp::Ordering;

use serde_json::{Map, Value, json};

use crate::schemas::insights::agent_report::AgentReport;
use crate::schemas::insights::analysis_candidate::AnalysisCandidate;
use crate::schemas::insights::model_insight::ModelInsight;

/// The request log records analysis runs under this client.
pub const ANALYSIS_CLIENT: &str = "llm-hub/insights-analysis";
/// The analysis reply is capped at this many tokens.
pub const ANALYSIS_MAX_OUTPUT_TOKENS: u64 = 1500;
/// A run estimated at or below this many US dollars counts as a fair cost.
pub const FAIR_RUN_USD: f64 = 0.05;
const CHARS_PER_TOKEN: u64 = 4;
const MODELS_IN_INPUT: usize = 15;
const LIST_IN_INPUT: usize = 5;
const ERRORS_PER_MODEL: usize = 2;
const TEMPERATURE: f64 = 0.2;
const MODEL_FIELDS: [&str; 22] = [
    "model",
    "ranked",
    "health",
    "use_cases",
    "requests",
    "success_rate_pct",
    "quality_score",
    "stability_score",
    "speed_score",
    "overall_score",
    "ttft_p50_ms",
    "decode_tokens_per_sec_p50",
    "cache_hit_rate_pct",
    "cost_usd",
    "cost_per_call_cents",
    "free",
    "value_score",
    "verified_pass_pct",
    "verified_outcomes",
    "sessions_verified",
    "sessions_solved",
    "best_harness",
];
const SYSTEM_PROMPT: &str = "You review the usage report of an LLM gateway that routes coding agents \
to many models. Use only the numbers in the report and never invent one. Scores are deterministic: \
read the `scoring` and `caveats` fields before you judge them. Write Markdown with these sections:\n\
## At a glance\nThree bullets: the main finding, the biggest risk, the best opportunity.\n\
## What works\nModels, harnesses and habits that do well, each with its evidence.\n\
## Friction\nFailures, slow paths, wasted spend and weak sessions, each with its evidence.\n\
## Which model for which job\nFor each job in the report, the model to use, a free option and a \
best-value option, with one reason each.\n\
## Try next\nAt most five concrete actions, most valuable first.\n\
Stay under 500 words.";

/// The report fields an analysis needs, trimmed so a small model can read them.
#[must_use]
pub fn compact_report(report: &AgentReport) -> Value {
    let models: Vec<Value> = report
        .models
        .iter()
        .filter(|model| model.requests > 0)
        .take(MODELS_IN_INPUT)
        .map(compact_model)
        .collect();
    let best_for: Vec<Value> = report
        .best_for
        .iter()
        .map(|pick| json!({"use_case": pick.use_case, "top": pick.top, "reason": pick.reason}))
        .collect();
    json!({
        "range": report.range,
        "scoring": report.scoring,
        "caveats": report.caveats,
        "summary": report.summary,
        "leaders": report.leaders,
        "best_for": best_for,
        "best_free": report.best_free,
        "best_value": report.best_value,
        "sessions": report.sessions,
        "harnesses": report.harnesses.iter().take(LIST_IN_INPUT).collect::<Vec<_>>(),
        "failure_reasons": report.failure_reasons.iter().take(LIST_IN_INPUT).collect::<Vec<_>>(),
        "models": models,
    })
}

fn compact_model(model: &ModelInsight) -> Value {
    let Ok(Value::Object(full)) = serde_json::to_value(model) else {
        return Value::Null;
    };
    let mut kept: Map<String, Value> = MODEL_FIELDS
        .iter()
        .filter_map(|field| {
            full.get(*field)
                .map(|value| ((*field).to_string(), value.clone()))
        })
        .collect();
    let errors: Vec<_> = model.top_errors.iter().take(ERRORS_PER_MODEL).collect();
    kept.insert("top_errors".into(), json!(errors));
    Value::Object(kept)
}

/// The prompt tokens a run sends, estimated from the input's length.
#[must_use]
pub fn estimate_input_tokens(input: &Value) -> u64 {
    let chars = input.to_string().len() + SYSTEM_PROMPT.len();
    u64::try_from(chars).unwrap_or(u64::MAX) / CHARS_PER_TOKEN
}

/// The non-streaming chat completion body of one run on `model` (a bare upstream model id).
#[must_use]
pub fn request_body(model: &str, input: &Value) -> Value {
    json!({
        "model": model,
        "stream": false,
        "temperature": TEMPERATURE,
        "max_tokens": ANALYSIS_MAX_OUTPUT_TOKENS,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": format!("Insights report (JSON):\n{input}")},
        ],
    })
}

/// The answer text of a chat completion reply, or `None` when it has none.
#[must_use]
pub fn reply_text(body: &[u8]) -> Option<String> {
    let reply: Value = serde_json::from_slice(body).ok()?;
    let text = reply
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)?
        .trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Ranked models offered for a run, recommended first, then by quality and cost.
/// `estimate_usd` prices one run on a model.
#[must_use]
pub fn candidates(
    models: &[ModelInsight],
    estimate_usd: impl Fn(&str) -> f64,
) -> Vec<AnalysisCandidate> {
    let mut offered: Vec<AnalysisCandidate> = models
        .iter()
        .filter(|model| model.ranked)
        .map(|model| {
            let estimated_cost_usd = if model.free {
                0.0
            } else {
                estimate_usd(&model.model)
            };
            AnalysisCandidate {
                model: model.model.clone(),
                quality_score: model.quality_score,
                free: model.free,
                estimated_cost_usd,
                recommended: false,
                reason: reason(model.free, estimated_cost_usd),
            }
        })
        .collect();
    offered.sort_by(by_quality_then_cost);
    if let Some(best) = offered
        .iter_mut()
        .find(|candidate| candidate.estimated_cost_usd <= FAIR_RUN_USD)
    {
        best.recommended = true;
        best.reason = format!("highest quality at a fair cost; {}", best.reason);
    }
    offered.sort_by(|a, b| {
        b.recommended
            .cmp(&a.recommended)
            .then_with(|| by_quality_then_cost(a, b))
    });
    offered
}

fn by_quality_then_cost(a: &AnalysisCandidate, b: &AnalysisCandidate) -> Ordering {
    b.quality_score
        .unwrap_or(0.0)
        .total_cmp(&a.quality_score.unwrap_or(0.0))
        .then_with(|| a.estimated_cost_usd.total_cmp(&b.estimated_cost_usd))
        .then_with(|| a.model.cmp(&b.model))
}

fn reason(free: bool, cost_usd: f64) -> String {
    if free {
        "free: runs on a local profile".to_string()
    } else if cost_usd <= FAIR_RUN_USD {
        format!("about ${cost_usd:.3} per run")
    } else {
        format!("about ${cost_usd:.3} per run, above the ${FAIR_RUN_USD:.2} fair limit")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::insights::error_count::ErrorCount;
    use crate::schemas::insights::insight_leaders::InsightLeaders;
    use crate::schemas::insights::model_health::ModelHealth;
    use crate::schemas::insights::sessions_insight::SessionsInsight;

    fn model(id: &str, quality: f64, free: bool, requests: u64) -> ModelInsight {
        ModelInsight {
            model: id.into(),
            profile: id.split('/').next().unwrap_or_default().into(),
            context_window: None,
            use_cases: Vec::new(),
            summary: Some("long registry note".into()),
            requests,
            errors: 0,
            success_rate_pct: 100.0,
            consecutive_failures: 0,
            health: ModelHealth::Healthy,
            ttft_p50_ms: Some(100),
            ttft_p95_ms: None,
            latency_p50_ms: None,
            decode_tokens_per_sec_p50: None,
            prefill_tokens_per_sec_p50: None,
            cache_hit_rate_pct: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            top_errors: vec![
                ErrorCount {
                    reason: "a".into(),
                    count: 3,
                },
                ErrorCount {
                    reason: "b".into(),
                    count: 2,
                },
                ErrorCount {
                    reason: "c".into(),
                    count: 1,
                },
            ],
            last_success_ms: None,
            last_error_ms: None,
            ranked: requests > 0,
            quality_score: Some(quality),
            stability_score: None,
            speed_score: None,
            overall_score: None,
            cost_usd: None,
            cost_per_call_cents: None,
            free,
            value_score: None,
            verified_outcomes: 0,
            verified_passed: 0,
            verified_pass_pct: None,
            sessions_verified: 0,
            sessions_solved: 0,
            by_harness: Vec::new(),
            best_harness: None,
        }
    }

    fn report(models: Vec<ModelInsight>) -> AgentReport {
        AgentReport {
            generated_at: "t".into(),
            range: "7d".into(),
            profile: None,
            min_requests: 5,
            scoring: "s".into(),
            caveats: vec!["c".into()],
            summary: vec!["line".into()],
            picks: Vec::new(),
            leaders: InsightLeaders::default(),
            best_for: Vec::new(),
            best_free: Vec::new(),
            best_value: Vec::new(),
            sessions: SessionsInsight::default(),
            harnesses: Vec::new(),
            failure_reasons: Vec::new(),
            models,
        }
    }

    #[test]
    fn compact_report_keeps_used_models_and_trims_their_fields() {
        let mut models: Vec<ModelInsight> = (0..20)
            .map(|i| model(&format!("p/m{i:02}"), 50.0, false, 3))
            .collect();
        models.push(model("p/idle", 50.0, false, 0));

        let compact = compact_report(&report(models));

        let kept = compact["models"].as_array().unwrap();
        assert_eq!(kept.len(), MODELS_IN_INPUT);
        assert!(kept[0].get("summary").is_none());
        assert!(kept[0].get("by_harness").is_none());
        assert_eq!(
            kept[0]["top_errors"].as_array().unwrap().len(),
            ERRORS_PER_MODEL
        );
        assert_eq!(kept[0]["quality_score"], 50.0);
        assert_eq!(compact["scoring"], "s");
        assert!(estimate_input_tokens(&compact) > 0);
    }

    #[test]
    fn request_body_is_one_capped_non_streaming_chat() {
        let body = request_body("small", &json!({"k": 1}));

        assert_eq!(body["model"], "small");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], ANALYSIS_MAX_OUTPUT_TOKENS);
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("{\"k\":1}")
        );
    }

    #[test]
    fn reply_text_reads_the_first_choice_or_nothing() {
        let ok = br#"{"choices":[{"message":{"content":"  ## Hi  "}}]}"#;
        assert_eq!(reply_text(ok), Some("## Hi".into()));
        assert_eq!(
            reply_text(br#"{"choices":[{"message":{"content":"  "}}]}"#),
            None
        );
        assert_eq!(reply_text(br#"{"error":{}}"#), None);
        assert_eq!(reply_text(b"not json"), None);
    }

    #[test]
    fn candidates_recommend_the_best_quality_within_the_fair_limit() {
        let models = vec![
            model("cloud/top", 97.0, false, 9),
            model("cloud/mid", 85.0, false, 9),
            model("local/free", 50.0, true, 9),
            model("cloud/unused", 99.0, false, 0),
        ];
        let price = |id: &str| match id {
            "cloud/top" => 0.20,
            "cloud/mid" => 0.03,
            _ => 1.0,
        };

        let offered = candidates(&models, price);

        let order: Vec<&str> = offered.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(order, vec!["cloud/mid", "cloud/top", "local/free"]);
        assert!(offered[0].recommended);
        assert!(
            offered[0]
                .reason
                .starts_with("highest quality at a fair cost")
        );
        assert!(offered[1].reason.contains("above the"));
        assert!(offered[2].estimated_cost_usd.abs() < f64::EPSILON);
        assert!(offered[2].reason.starts_with("free"));
    }

    #[test]
    fn candidates_fall_back_to_a_free_model_when_every_paid_one_is_dear() {
        let models = vec![
            model("cloud/top", 97.0, false, 9),
            model("local/free", 50.0, true, 9),
        ];

        let offered = candidates(&models, |_| 1.0);

        assert_eq!(offered[0].model, "local/free");
        assert!(offered[0].recommended);
        assert!(!offered[1].recommended);
    }
}

//! Per-model speed, stability and health derived from the request log, plus
//! "which model should I use" picks built on top of them.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::consts::ERROR_STATUS_MIN;
use crate::schemas::insights::agent_report::AgentReport;
use crate::schemas::insights::error_count::ErrorCount;
use crate::schemas::insights::failure_reason::FailureReason;
use crate::schemas::insights::harness_insight::HarnessInsight;
use crate::schemas::insights::insight_leaders::InsightLeaders;
use crate::schemas::insights::insights_report::InsightsReport;
use crate::schemas::insights::model_catalog_entry::ModelCatalogEntry;
use crate::schemas::insights::model_cost::ModelCost;
use crate::schemas::insights::model_health::ModelHealth;
use crate::schemas::insights::model_insight::ModelInsight;
use crate::schemas::insights::model_pick::ModelPick;
use crate::schemas::insights::pick_by::PickBy;
use crate::schemas::insights::pick_candidate::PickCandidate;
use crate::schemas::insights::use_case_pick::UseCasePick;
use crate::schemas::model_context::{max_input_tokens_from_item, strip_1m_suffix};
use crate::schemas::work_role::WorkRole;
use crate::services::harness::{classify, is_keep_warm};
use crate::services::pricing::split_cached_input;
use crate::services::smart_auto_suitability::SmartAutoSuitability;
use crate::services::stats::{RequestOutcome, tokens_per_sec};
use crate::utils::time::utc_iso8601;

/// This many newest calls failing in a row marks a model as failing.
const FAILING_STREAK: u64 = 3;
/// Below this success rate a model is degraded.
const DEGRADED_SUCCESS_PCT: f64 = 90.0;
const QUALITY_WEIGHT: f64 = 0.5;
const STABILITY_WEIGHT: f64 = 0.3;
const SPEED_WEIGHT: f64 = 0.2;
/// z for a 95% Wilson lower bound: few calls cannot prove high reliability.
const WILSON_Z: f64 = 1.96;
const TOP_ERRORS: usize = 3;
const PICK_ALTERNATIVES: usize = 4;
/// Models listed per top list: best free, best value, and per use case.
const TOP_PICKS: usize = 3;
const PERCENT: f64 = 100.0;
const MEDIAN: f64 = 0.5;
const P95: f64 = 0.95;
const AGENT_FAILURE_REASONS: usize = 10;
/// A harness needs this many calls on a model before it can be its best harness.
const HARNESS_MIN_CALLS: u64 = 3;
/// Probes and scripts, not agent harnesses: never offered as a model's best harness.
const NON_AGENT_CLIENTS: [&str; 3] = ["curl", "python", "unknown"];
const MS_PER_SECOND: f64 = 1000.0;
const SCORING: &str = "overall = 0.5 x quality + 0.3 x reliability + 0.2 x speed; quality = public \
capability tier of the model family (unknown families score 50); reliability = 95% lower bound of the \
success rate, so few calls score lower; speed = half output tok/s relative to the fastest ranked model + \
half the fastest first-token p50 relative to this model's. Use cases mapped to a role score that role's \
quality instead";
const CAVEATS: [&str; 3] = [
    "Quality comes from a fixed table of public capability tiers per model family, not from graded \
answers: an unknown family scores average.",
    "First-token time includes cold model loads; a single-slot local router holds one model at a time, \
so switching models costs a reload.",
    "Output speed of very short replies is dominated by overhead; compare models on similar workloads.",
];

/// Estimated spend and free flag per model key.
pub type ModelCosts = HashMap<String, ModelCost>;

/// Builds the insights report from outcomes in chronological order.
///
/// `costs` adds spend, cost per call, free models, value and the top-three lists;
/// `catalog` adds context window and declared use cases, and lists models that
/// had no calls in the window so readers still see them.
#[must_use]
pub fn build_report_priced(
    outcomes: &[RequestOutcome],
    catalog: &HashMap<String, ModelCatalogEntry>,
    range: &str,
    min_requests: u64,
    generated_ms: u64,
    costs: &ModelCosts,
) -> InsightsReport {
    let calls: Vec<&RequestOutcome> = outcomes
        .iter()
        .filter(|outcome| !is_keep_warm(outcome.client.as_deref()))
        .collect();
    let mut by_model: HashMap<&str, Vec<&RequestOutcome>> = HashMap::new();
    for &outcome in &calls {
        by_model
            .entry(&outcome.model_key)
            .or_default()
            .push(outcome);
    }
    let mut models: Vec<ModelInsight> = by_model
        .into_values()
        .map(|calls| model_insight(&calls, min_requests))
        .collect();
    for id in catalog.keys() {
        if !models.iter().any(|model| model.model == *id) {
            models.push(unused_model(id));
        }
    }
    for model in &mut models {
        model.quality_score = Some(round_score(SmartAutoSuitability::quality_pct(
            &model.model,
            None,
        )));
        if let Some(entry) = catalog.get(&model.model) {
            model.context_window = entry.context_window;
            model.use_cases.clone_from(&entry.use_cases);
            model.summary.clone_from(&entry.summary);
        }
    }
    for model in &mut models {
        if let Some(cost) = costs.get(&model.model) {
            model.cost_usd = Some(cost.usd);
            model.free = cost.free;
        }
    }
    score_ranked(&mut models);
    score_value(&mut models);
    models.sort_by(report_order);
    InsightsReport {
        range: range.to_string(),
        min_requests,
        generated_ms,
        leaders: leaders(&models),
        best_for: best_for(&models),
        best_free: top_models(
            &models,
            |model| model.ranked && model.free,
            |model| model.overall_score,
        ),
        best_value: top_models(&models, |model| model.ranked, |model| model.value_score),
        harnesses: harness_insights(&calls),
        models,
    }
}

/// Context window, use cases and summary per hub model id, read from a
/// `/v1/models` payload. llama-swap rows carry the registry's `use_cases` and
/// `summary` in `meta.llamaswap`.
#[must_use]
pub fn catalog_from_models(payload: &Value) -> HashMap<String, ModelCatalogEntry> {
    let Some(items) = payload.get("data").and_then(Value::as_array) else {
        return HashMap::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            let meta = item.get("meta").and_then(|meta| meta.get("llamaswap"));
            let use_cases = meta
                .and_then(|meta| meta.get("use_cases"))
                .map(string_list)
                .unwrap_or_default();
            let summary = meta
                .and_then(|meta| meta.get("summary"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            let entry = ModelCatalogEntry {
                context_window: max_input_tokens_from_item(item),
                use_cases,
                summary,
            };
            Some((strip_1m_suffix(id).to_string(), entry))
        })
        .collect()
}

fn string_list(value: &Value) -> Vec<String> {
    let raw: Vec<String> = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(text) => text.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    raw.into_iter()
        .map(|item| item.trim().to_ascii_lowercase())
        .filter(|item| !item.is_empty())
        .collect()
}

/// The best ranked model for `by`, with runners-up; `None` when nothing is ranked.
#[must_use]
pub fn pick(report: &InsightsReport, by: PickBy) -> Option<ModelPick> {
    let mut candidates: Vec<(&ModelInsight, f64)> = report
        .models
        .iter()
        .filter(|model| model.ranked)
        .filter_map(|model| criterion_value(model, by).map(|value| (model, value)))
        .collect();
    let lower_is_better = by == PickBy::Ttft;
    candidates.sort_by(|a, b| {
        let order = compare_f64(a.1, b.1);
        if lower_is_better {
            order
        } else {
            order.reverse()
        }
    });
    let (winner, value) = *candidates.first()?;
    let alternatives = candidates
        .iter()
        .skip(1)
        .take(PICK_ALTERNATIVES)
        .map(|(model, value)| PickCandidate {
            model: model.model.clone(),
            value: *value,
        })
        .collect();
    Some(ModelPick {
        by,
        model: winner.model.clone(),
        value,
        reason: pick_reason(winner, by, value, &report.range),
        alternatives,
    })
}

/// Everything an agent needs to choose a model, in one document.
/// `costs` as in [`build_report_priced`].
#[must_use]
pub fn build_agent_report_priced(
    outcomes: &[RequestOutcome],
    catalog: &HashMap<String, ModelCatalogEntry>,
    range: &str,
    profile: Option<String>,
    min_requests: u64,
    generated_ms: u64,
    costs: &ModelCosts,
) -> AgentReport {
    let report = build_report_priced(outcomes, catalog, range, min_requests, generated_ms, costs);
    let picks: Vec<ModelPick> = PickBy::ALL
        .into_iter()
        .filter_map(|by| pick(&report, by))
        .collect();
    AgentReport {
        generated_at: utc_iso8601(generated_ms),
        range: report.range.clone(),
        profile,
        min_requests,
        scoring: SCORING.to_string(),
        caveats: CAVEATS.iter().map(ToString::to_string).collect(),
        summary: summary(&report, &picks),
        picks,
        leaders: report.leaders,
        best_for: report.best_for,
        best_free: report.best_free,
        best_value: report.best_value,
        harnesses: report.harnesses,
        failure_reasons: failure_reasons(outcomes),
        models: report.models,
    }
}

fn summary(report: &InsightsReport, picks: &[ModelPick]) -> Vec<String> {
    let mut lines = Vec::new();
    let by_id: HashMap<&str, &ModelInsight> = report
        .models
        .iter()
        .map(|model| (model.model.as_str(), model))
        .collect();
    match picks.iter().find(|picked| picked.by == PickBy::Overall) {
        Some(best) => {
            if let Some(model) = by_id.get(best.model.as_str()) {
                lines.push(format!(
                    "Best overall: {} ({}).",
                    best.model,
                    describe(model)
                ));
            }
        }
        None => lines.push(format!(
            "No model has {} or more calls in {} without a failure streak, so nothing is ranked. \
Lower min_requests or widen the range.",
            report.min_requests, report.range
        )),
    }
    for picked in picks.iter().filter(|picked| picked.by != PickBy::Overall) {
        lines.push(format!("{}: {}.", criterion_label(picked.by), picked.model));
    }
    if !report.best_free.is_empty() {
        lines.push(format!("Best free: {}.", report.best_free.join(", ")));
    }
    if !report.best_value.is_empty() {
        lines.push(format!("Best value: {}.", report.best_value.join(", ")));
    }
    for use_case_pick in &report.best_for {
        let basis = if use_case_pick.ranked {
            "from call stats"
        } else {
            "declared only, too few calls to rank"
        };
        lines.push(format!(
            "Best for {}: {} ({basis}).",
            use_case_pick.use_case, use_case_pick.model
        ));
    }
    for model in report.models.iter().filter(|model| model.requests > 0) {
        if let Some(harness) = &model.best_harness {
            lines.push(format!("{} works best in {harness}.", model.model));
        }
    }
    for model in &report.models {
        let reason = model.top_errors.first().map_or_else(String::new, |error| {
            format!(" Most common error: {}", error.reason)
        });
        match model.health {
            ModelHealth::Failing => lines.push(format!(
                "{} is failing now: its last {} calls failed.{reason}",
                model.model, model.consecutive_failures
            )),
            ModelHealth::Degraded => lines.push(format!(
                "{} is degraded: {:.1}% success over {} calls.{reason}",
                model.model, model.success_rate_pct, model.requests
            )),
            ModelHealth::Healthy | ModelHealth::InsufficientData => {}
        }
    }
    let too_few: Vec<&str> = report
        .models
        .iter()
        .filter(|model| model.health == ModelHealth::InsufficientData)
        .map(|model| model.model.as_str())
        .collect();
    if !too_few.is_empty() {
        lines.push(format!(
            "Not ranked, fewer than {} calls: {}.",
            report.min_requests,
            too_few.join(", ")
        ));
    }
    lines
}

fn describe(model: &ModelInsight) -> String {
    let mut parts = vec![format!(
        "{:.1}% success over {} calls",
        model.success_rate_pct, model.requests
    )];
    if let Some(ttft) = model.ttft_p50_ms {
        parts.push(format!("first token p50 {}", human_ms(ttft)));
    }
    if let Some(decode) = model.decode_tokens_per_sec_p50 {
        parts.push(format!("output {decode:.1} tok/s"));
    }
    if let Some(score) = model.overall_score {
        parts.push(format!("overall score {score:.1}"));
    }
    parts.join(", ")
}

fn criterion_label(by: PickBy) -> &'static str {
    match by {
        PickBy::Overall => "Best overall",
        PickBy::Stability => "Most reliable",
        PickBy::Speed => "Fastest overall",
        PickBy::Ttft => "Quickest first token",
        PickBy::Decode => "Fastest output",
        PickBy::Prefill => "Fastest prompt reading",
    }
}

/// Running totals for one failure reason while grouping.
#[derive(Default)]
struct ReasonTally {
    count: u64,
    models: BTreeSet<String>,
    last_ms: u64,
}

fn failure_reasons(outcomes: &[RequestOutcome]) -> Vec<FailureReason> {
    let mut tallies: HashMap<String, ReasonTally> = HashMap::new();
    for call in outcomes.iter().filter(|call| !is_success(call)) {
        let tally = tallies.entry(failure_reason(call)).or_default();
        tally.count += 1;
        tally.models.insert(call.model_key.clone());
        tally.last_ms = tally.last_ms.max(call.ts_ms);
    }
    let mut reasons: Vec<(String, ReasonTally)> = tallies.into_iter().collect();
    reasons.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then_with(|| b.1.last_ms.cmp(&a.1.last_ms))
    });
    reasons
        .into_iter()
        .take(AGENT_FAILURE_REASONS)
        .map(|(reason, tally)| FailureReason {
            reason,
            count: tally.count,
            models: tally.models.into_iter().collect(),
            last_seen: utc_iso8601(tally.last_ms),
        })
        .collect()
}

fn failure_reason(call: &RequestOutcome) -> String {
    call.error
        .clone()
        .unwrap_or_else(|| format!("HTTP {}", call.status))
}

fn human_ms(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    format!("{:.1} s", to_f64(ms) / MS_PER_SECOND)
}

fn model_insight(calls: &[&RequestOutcome], min_requests: u64) -> ModelInsight {
    let successes: Vec<&RequestOutcome> = calls
        .iter()
        .copied()
        .filter(|call| is_success(call))
        .collect();
    let requests = calls.len() as u64;
    let errors = requests - successes.len() as u64;
    let success_rate_pct = ratio_pct(successes.len() as u64, requests);
    let consecutive_failures = calls
        .iter()
        .rev()
        .take_while(|call| !is_success(call))
        .count() as u64;

    let ttfts: Vec<f64> = successes
        .iter()
        .filter_map(|call| call.ttft_ms)
        .map(to_f64)
        .collect();
    let latencies: Vec<f64> = successes
        .iter()
        .map(|call| to_f64(call.latency_ms))
        .collect();
    let decode: Vec<f64> = successes
        .iter()
        .filter_map(|call| tokens_per_sec(call))
        .collect();
    let prefill: Vec<f64> = successes
        .iter()
        .filter_map(|call| prefill_tokens_per_sec(call))
        .collect();
    let tokens_in: u64 = successes.iter().map(|call| call.tokens_in).sum();
    let (uncached_in, cached_in) = successes
        .iter()
        .map(|call| {
            split_cached_input(
                call.tokens_in,
                call.cache_read_tokens,
                call.cache_write_tokens,
            )
        })
        .fold(
            (0, 0),
            |(uncached, cached), (call_uncached, call_cached)| {
                (uncached + call_uncached, cached + call_cached)
            },
        );

    let health = health(
        requests,
        success_rate_pct,
        consecutive_failures,
        min_requests,
    );
    let first = calls.first().copied();
    let by_harness = harness_insights(calls);
    ModelInsight {
        model: first.map(|call| call.model_key.clone()).unwrap_or_default(),
        profile: first.map(|call| call.profile.clone()).unwrap_or_default(),
        context_window: None,
        use_cases: Vec::new(),
        summary: None,
        requests,
        errors,
        success_rate_pct,
        consecutive_failures,
        health,
        ttft_p50_ms: percentile(&ttfts, MEDIAN).map(round_ms),
        ttft_p95_ms: percentile(&ttfts, P95).map(round_ms),
        latency_p50_ms: percentile(&latencies, MEDIAN).map(round_ms),
        decode_tokens_per_sec_p50: percentile(&decode, MEDIAN),
        prefill_tokens_per_sec_p50: percentile(&prefill, MEDIAN),
        cache_hit_rate_pct: ratio_pct(cached_in, uncached_in + cached_in),
        tokens_in,
        tokens_out: successes.iter().map(|call| call.tokens_out).sum(),
        top_errors: top_errors(calls),
        last_success_ms: successes.last().map(|call| call.ts_ms),
        last_error_ms: calls
            .iter()
            .rev()
            .find(|call| !is_success(call))
            .map(|call| call.ts_ms),
        // Speech models return no output tokens, so they never compete with chat models.
        ranked: requests >= min_requests
            && health != ModelHealth::Failing
            && successes.iter().any(|call| call.tokens_out > 0),
        quality_score: None,
        stability_score: None,
        speed_score: None,
        overall_score: None,
        cost_usd: None,
        cost_per_call_cents: None,
        free: false,
        value_score: None,
        best_harness: best_harness(&by_harness),
        by_harness,
    }
}

/// A catalog model with no calls in the window.
fn unused_model(id: &str) -> ModelInsight {
    ModelInsight {
        model: id.to_string(),
        profile: id.split('/').next().unwrap_or_default().to_string(),
        context_window: None,
        use_cases: Vec::new(),
        summary: None,
        requests: 0,
        errors: 0,
        success_rate_pct: 0.0,
        consecutive_failures: 0,
        health: ModelHealth::InsufficientData,
        ttft_p50_ms: None,
        ttft_p95_ms: None,
        latency_p50_ms: None,
        decode_tokens_per_sec_p50: None,
        prefill_tokens_per_sec_p50: None,
        cache_hit_rate_pct: 0.0,
        tokens_in: 0,
        tokens_out: 0,
        top_errors: Vec::new(),
        last_success_ms: None,
        last_error_ms: None,
        ranked: false,
        quality_score: None,
        stability_score: None,
        speed_score: None,
        overall_score: None,
        cost_usd: None,
        cost_per_call_cents: None,
        free: false,
        value_score: None,
        by_harness: Vec::new(),
        best_harness: None,
    }
}

/// Success rate and speed per client harness, busiest harness first.
fn harness_insights(calls: &[&RequestOutcome]) -> Vec<HarnessInsight> {
    let mut groups: HashMap<String, Vec<&RequestOutcome>> = HashMap::new();
    for call in calls {
        groups
            .entry(classify(call.client.as_deref()))
            .or_default()
            .push(call);
    }
    let mut insights: Vec<HarnessInsight> = groups
        .into_iter()
        .map(|(harness, group)| {
            let successes: Vec<&RequestOutcome> = group
                .iter()
                .copied()
                .filter(|call| is_success(call))
                .collect();
            let ttfts: Vec<f64> = successes
                .iter()
                .filter_map(|call| call.ttft_ms)
                .map(to_f64)
                .collect();
            let decode: Vec<f64> = successes
                .iter()
                .filter_map(|call| tokens_per_sec(call))
                .collect();
            HarnessInsight {
                harness,
                requests: group.len() as u64,
                success_rate_pct: ratio_pct(successes.len() as u64, group.len() as u64),
                ttft_p50_ms: percentile(&ttfts, MEDIAN).map(round_ms),
                decode_tokens_per_sec_p50: percentile(&decode, MEDIAN),
            }
        })
        .collect();
    insights.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.harness.cmp(&b.harness))
    });
    insights
}

/// Highest success rate among agent harnesses with enough calls; lower TTFT
/// then more calls break ties.
fn best_harness(by_harness: &[HarnessInsight]) -> Option<String> {
    by_harness
        .iter()
        .filter(|entry| entry.requests >= HARNESS_MIN_CALLS)
        .filter(|entry| !NON_AGENT_CLIENTS.contains(&entry.harness.as_str()))
        .max_by(|a, b| {
            compare_f64(a.success_rate_pct, b.success_rate_pct)
                .then_with(|| {
                    b.ttft_p50_ms
                        .unwrap_or(u64::MAX)
                        .cmp(&a.ttft_p50_ms.unwrap_or(u64::MAX))
                })
                .then_with(|| a.requests.cmp(&b.requests))
        })
        .map(|entry| entry.harness.clone())
}

/// How a declared use case ranks its candidates.
#[derive(Clone, Copy)]
enum UseCaseRank {
    /// Overall score with this role's seed quality; every ranked model competes.
    Role(WorkRole),
    /// Speed score; every ranked model competes.
    Speed,
    /// Overall score among the tagged models only.
    TaggedOnly,
}

impl UseCaseRank {
    fn of(use_case: &str) -> Self {
        match use_case {
            "coding" => Self::Role(WorkRole::Implementation),
            "planning" | "reasoning" => Self::Role(WorkRole::Planning),
            "general" | "chat" => Self::Role(WorkRole::Conversation),
            "review" => Self::Role(WorkRole::Validation),
            "fast" => Self::Speed,
            _ => Self::TaggedOnly,
        }
    }

    fn score(self, model: &ModelInsight) -> Option<f64> {
        match self {
            Self::Role(role) => Some(round_score(weighted(
                SmartAutoSuitability::quality_pct(&model.model, Some(role)),
                model.stability_score?,
                model.speed_score?,
            ))),
            Self::Speed => model.speed_score,
            Self::TaggedOnly => model.overall_score,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Role(_) => "use-case score",
            Self::Speed => "speed score",
            Self::TaggedOnly => "overall score",
        }
    }
}

/// One pick per declared use case. Use cases that map to a role or to speed rank every
/// ranked model, so a cloud model can win a use case only local models declare. Other
/// use cases rank the tagged models. With nothing ranked, the tagged model with the most
/// calls wins.
fn best_for(models: &[ModelInsight]) -> Vec<UseCasePick> {
    let use_cases: BTreeSet<&str> = models
        .iter()
        .flat_map(|model| model.use_cases.iter().map(String::as_str))
        .collect();
    use_cases
        .into_iter()
        .filter_map(|use_case| {
            let rank = UseCaseRank::of(use_case);
            let tagged: Vec<&ModelInsight> = models
                .iter()
                .filter(|model| model.use_cases.iter().any(|tag| tag == use_case))
                .collect();
            let open = !matches!(rank, UseCaseRank::TaggedOnly);
            let candidates: Vec<&ModelInsight> = models
                .iter()
                .filter(|model| {
                    model.use_cases.iter().any(|tag| tag == use_case) || (open && model.ranked)
                })
                .collect();
            let top = top_for(&candidates, &tagged, rank);
            let winner = models.iter().find(|model| Some(&model.model) == top.first())?;
            let ranked = winner.ranked;
            let reason = if ranked {
                let scope = if open {
                    "all ranked models"
                } else {
                    "models declared for this use case"
                };
                format!(
                    "highest {} ({:.1}) for {use_case} among {scope}",
                    rank.label(),
                    rank.score(winner).unwrap_or(0.0)
                )
            } else {
                format!(
                    "declared for {use_case} in the model registry; no tagged model has enough calls to rank"
                )
            };
            Some(UseCasePick {
                use_case: use_case.to_string(),
                model: winner.model.clone(),
                ranked,
                reason,
                candidates: candidates.iter().map(|model| model.model.clone()).collect(),
                top,
            })
        })
        .collect()
}

fn health(
    requests: u64,
    success_rate_pct: f64,
    consecutive_failures: u64,
    min_requests: u64,
) -> ModelHealth {
    if consecutive_failures >= FAILING_STREAK {
        return ModelHealth::Failing;
    }
    if requests < min_requests {
        return ModelHealth::InsufficientData;
    }
    if success_rate_pct < DEGRADED_SUCCESS_PCT {
        return ModelHealth::Degraded;
    }
    ModelHealth::Healthy
}

/// Speed is relative to the other ranked models: the fastest decode and the
/// lowest TTFT each earn full marks for their half.
fn score_ranked(models: &mut [ModelInsight]) {
    let ranked = || models.iter().filter(|model| model.ranked);
    let max_decode = ranked()
        .filter_map(|model| model.decode_tokens_per_sec_p50)
        .fold(0.0, f64::max);
    let min_ttft = ranked()
        .filter_map(|model| model.ttft_p50_ms)
        .filter(|ttft| *ttft > 0)
        .min();
    for model in models.iter_mut().filter(|model| model.ranked) {
        let decode_share = match model.decode_tokens_per_sec_p50 {
            Some(decode) if max_decode > 0.0 => decode / max_decode,
            _ => 0.0,
        };
        let ttft_share = match (min_ttft, model.ttft_p50_ms) {
            (Some(best), Some(ttft)) if ttft > 0 => to_f64(best) / to_f64(ttft),
            _ => 0.0,
        };
        let speed = PERCENT * (decode_share + ttft_share) / 2.0;
        let stability = wilson_lower_pct(model.requests - model.errors, model.requests);
        let quality = model.quality_score.unwrap_or(0.0);
        model.speed_score = Some(round_score(speed));
        model.stability_score = Some(round_score(stability));
        model.overall_score = Some(round_score(weighted(quality, stability, speed)));
    }
}

fn weighted(quality: f64, stability: f64, speed: f64) -> f64 {
    QUALITY_WEIGHT * quality + STABILITY_WEIGHT * stability + SPEED_WEIGHT * speed
}

/// The 95% Wilson lower bound of `successes / requests`, as a percentage.
fn wilson_lower_pct(successes: u64, requests: u64) -> f64 {
    if requests == 0 {
        return 0.0;
    }
    let n = to_f64(requests);
    let p = to_f64(successes) / n;
    let z2 = WILSON_Z * WILSON_Z;
    let centre = p + z2 / (2.0 * n);
    let spread = WILSON_Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    PERCENT * ((centre - spread) / (1.0 + z2 / n)).max(0.0)
}

/// Value is the overall score per US cent per successful call, for paid ranked models.
fn score_value(models: &mut [ModelInsight]) {
    for model in models
        .iter_mut()
        .filter(|model| model.ranked && !model.free)
    {
        let successes = model.requests - model.errors;
        let (Some(usd), Some(overall)) = (model.cost_usd, model.overall_score) else {
            continue;
        };
        if usd <= 0.0 || successes == 0 {
            continue;
        }
        let cents_per_call = PERCENT * usd / to_f64(successes);
        model.cost_per_call_cents = Some(round_cost(cents_per_call));
        model.value_score = Some(round_score(overall / cents_per_call));
    }
}

/// The top `TOP_PICKS` models that pass `keep`, highest `value` first, ties by name.
fn top_models(
    models: &[ModelInsight],
    keep: impl Fn(&ModelInsight) -> bool,
    value: impl Fn(&ModelInsight) -> Option<f64>,
) -> Vec<String> {
    let mut candidates: Vec<(&str, f64)> = models
        .iter()
        .filter(|model| keep(model))
        .filter_map(|model| value(model).map(|v| (model.model.as_str(), v)))
        .collect();
    candidates.sort_by(|a, b| compare_f64(b.1, a.1).then_with(|| a.0.cmp(b.0)));
    candidates
        .into_iter()
        .take(TOP_PICKS)
        .map(|(model, _)| model.to_string())
        .collect()
}

/// Top models for one use case: the ranked candidates by `rank`, else the tagged models by calls.
fn top_for(
    candidates: &[&ModelInsight],
    tagged: &[&ModelInsight],
    rank: UseCaseRank,
) -> Vec<String> {
    let mut scored: Vec<(&ModelInsight, f64)> = candidates
        .iter()
        .filter(|model| model.ranked)
        .filter_map(|model| rank.score(model).map(|score| (*model, score)))
        .collect();
    if scored.is_empty() {
        let mut by_calls: Vec<&&ModelInsight> = tagged.iter().collect();
        by_calls.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then_with(|| a.model.cmp(&b.model))
        });
        return by_calls
            .into_iter()
            .take(TOP_PICKS)
            .map(|model| model.model.clone())
            .collect();
    }
    scored.sort_by(|a, b| compare_f64(b.1, a.1).then_with(|| a.0.model.cmp(&b.0.model)));
    scored
        .into_iter()
        .take(TOP_PICKS)
        .map(|(model, _)| model.model.clone())
        .collect()
}

fn leaders(models: &[ModelInsight]) -> InsightLeaders {
    let ranked: Vec<&ModelInsight> = models.iter().filter(|model| model.ranked).collect();
    let top = |value: fn(&ModelInsight) -> Option<f64>| -> Vec<String> {
        let mut candidates: Vec<(&ModelInsight, f64)> = ranked
            .iter()
            .filter_map(|model| value(model).map(|v| (*model, v)))
            .collect();
        candidates.sort_by(|a, b| {
            compare_f64(b.1, a.1)
                .then_with(|| {
                    compare_f64(
                        b.0.overall_score.unwrap_or(0.0),
                        a.0.overall_score.unwrap_or(0.0),
                    )
                })
                .then_with(|| b.0.requests.cmp(&a.0.requests))
                .then_with(|| a.0.model.cmp(&b.0.model))
        });
        candidates
            .into_iter()
            .take(TOP_PICKS)
            .map(|(model, _)| model.model.clone())
            .collect()
    };
    let mut by_calls: Vec<&ModelInsight> = models.iter().collect();
    by_calls.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.model.cmp(&b.model))
    });
    InsightLeaders {
        best_overall: top(|model| model.overall_score),
        most_stable: top(|model| model.stability_score),
        fastest_first_token: top(|model| model.ttft_p50_ms.map(|ttft| -to_f64(ttft))),
        fastest_decode: top(|model| model.decode_tokens_per_sec_p50),
        fastest_prefill: top(|model| model.prefill_tokens_per_sec_p50),
        most_used: by_calls
            .into_iter()
            .take(TOP_PICKS)
            .filter(|model| model.requests > 0)
            .map(|model| model.model.clone())
            .collect(),
    }
}

fn criterion_value(model: &ModelInsight, by: PickBy) -> Option<f64> {
    match by {
        PickBy::Overall => model.overall_score,
        PickBy::Stability => model.stability_score,
        PickBy::Speed => model.speed_score,
        PickBy::Ttft => model.ttft_p50_ms.map(to_f64),
        PickBy::Decode => model.decode_tokens_per_sec_p50,
        PickBy::Prefill => model.prefill_tokens_per_sec_p50,
    }
}

fn pick_reason(model: &ModelInsight, by: PickBy, value: f64, range: &str) -> String {
    let criterion = match by {
        PickBy::Overall => format!("overall score {value:.1}"),
        PickBy::Stability => format!("stability score {value:.1}"),
        PickBy::Speed => format!("speed score {value:.1}"),
        PickBy::Ttft => format!("lowest TTFT p50 {value:.0} ms"),
        PickBy::Decode => format!("fastest decode {value:.1} tok/s"),
        PickBy::Prefill => format!("fastest prefill {value:.0} tok/s"),
    };
    format!(
        "{criterion}; {:.1}% success over {} calls in {range}",
        model.success_rate_pct, model.requests
    )
}

fn report_order(a: &ModelInsight, b: &ModelInsight) -> Ordering {
    b.ranked
        .cmp(&a.ranked)
        .then_with(|| {
            compare_f64(
                b.overall_score.unwrap_or(0.0),
                a.overall_score.unwrap_or(0.0),
            )
        })
        .then_with(|| b.requests.cmp(&a.requests))
        .then_with(|| a.model.cmp(&b.model))
}

fn top_errors(calls: &[&RequestOutcome]) -> Vec<ErrorCount> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for call in calls.iter().filter(|call| !is_success(call)) {
        *counts.entry(failure_reason(call)).or_default() += 1;
    }
    let mut sorted: Vec<ErrorCount> = counts
        .into_iter()
        .map(|(reason, count)| ErrorCount { reason, count })
        .collect();
    sorted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    sorted.truncate(TOP_ERRORS);
    sorted
}

/// Uncached prompt tokens processed per second before the first token.
fn prefill_tokens_per_sec(call: &RequestOutcome) -> Option<f64> {
    let ttft = call.ttft_ms.filter(|ttft| *ttft > 0)?;
    let (fresh, _) = split_cached_input(
        call.tokens_in,
        call.cache_read_tokens,
        call.cache_write_tokens,
    );
    (fresh > 0).then(|| to_f64(fresh) * 1000.0 / to_f64(ttft))
}

fn is_success(call: &RequestOutcome) -> bool {
    call.status < ERROR_STATUS_MIN
}

/// Nearest-rank percentile; `None` for an empty sample.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Rank is within 0..=len"
)]
fn percentile(values: &[f64], quantile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| compare_f64(*a, *b));
    let rank = (quantile * to_f64(sorted.len() as u64)).ceil();
    let index = (rank as usize).clamp(1, sorted.len()) - 1;
    Some(sorted[index])
}

fn ratio_pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    round_score(PERCENT * to_f64(part) / to_f64(whole))
}

fn compare_f64(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Token and ms counts stay far below 2^52"
)]
fn to_f64(value: u64) -> f64 {
    value as f64
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Non-negative ms values"
)]
fn round_ms(value: f64) -> u64 {
    value.round() as u64
}

fn round_score(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Four decimals: a cent per call can be a fraction of a cent.
fn round_cost(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_000_000;

    fn call(model: &str, status: u16, ttft_ms: u64, latency_ms: u64) -> RequestOutcome {
        RequestOutcome {
            profile: model.split('/').next().unwrap_or_default().to_string(),
            model_key: model.to_string(),
            status,
            ts_ms: 0,
            latency_ms,
            ttft_ms: Some(ttft_ms),
            tokens_in: 1000,
            tokens_out: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: (status >= ERROR_STATUS_MIN).then(|| format!("boom {status}")),
            client: None,
            worker: None,
            routed_by: None,
            attempt: None,
            role: None,
        }
    }

    fn ok(model: &str, ttft_ms: u64, latency_ms: u64) -> RequestOutcome {
        call(model, 200, ttft_ms, latency_ms)
    }

    fn fail(model: &str) -> RequestOutcome {
        call(model, 500, 10, 10)
    }

    fn timed(mut calls: Vec<RequestOutcome>) -> Vec<RequestOutcome> {
        for (index, outcome) in calls.iter_mut().enumerate() {
            outcome.ts_ms = index as u64 + 1;
        }
        calls
    }

    fn insight<'a>(report: &'a InsightsReport, model: &str) -> &'a ModelInsight {
        report.models.iter().find(|m| m.model == model).unwrap()
    }

    fn build_report(
        outcomes: &[RequestOutcome],
        catalog: &HashMap<String, ModelCatalogEntry>,
        range: &str,
        min_requests: u64,
        generated_ms: u64,
    ) -> InsightsReport {
        build_report_priced(
            outcomes,
            catalog,
            range,
            min_requests,
            generated_ms,
            &ModelCosts::new(),
        )
    }

    fn build_agent_report(
        outcomes: &[RequestOutcome],
        catalog: &HashMap<String, ModelCatalogEntry>,
        range: &str,
        profile: Option<String>,
        min_requests: u64,
        generated_ms: u64,
    ) -> AgentReport {
        build_agent_report_priced(
            outcomes,
            catalog,
            range,
            profile,
            min_requests,
            generated_ms,
            &ModelCosts::new(),
        )
    }

    #[test]
    fn metrics_computed_from_successful_calls() {
        let mut cached = ok("p/a", 500, 1500);
        cached.cache_read_tokens = 500;
        let outcomes = timed(vec![ok("p/a", 1000, 2000), cached, fail("p/a")]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);
        let a = insight(&report, "p/a");

        assert_eq!((a.requests, a.errors), (3, 1));
        assert!((a.success_rate_pct - 66.7).abs() < 1e-9);
        assert_eq!(a.ttft_p50_ms, Some(500));
        assert_eq!(a.latency_p50_ms, Some(1500));
        assert_eq!(a.decode_tokens_per_sec_p50, Some(100.0));
        assert_eq!(a.prefill_tokens_per_sec_p50, Some(1000.0));
        assert!((a.cache_hit_rate_pct - 25.0).abs() < 1e-9);
        assert_eq!(
            a.top_errors,
            vec![ErrorCount {
                reason: "boom 500".into(),
                count: 1
            }]
        );
        assert_eq!((a.last_success_ms, a.last_error_ms), (Some(2), Some(3)));
    }

    #[test]
    fn cache_hit_rate_counts_subset_and_separate_cache_reads() {
        let mut openai = ok("p/a", 1000, 2000);
        openai.cache_read_tokens = 500;
        let mut anthropic = ok("p/a", 1000, 2000);
        anthropic.tokens_in = 200;
        anthropic.cache_read_tokens = 800;
        let outcomes = timed(vec![openai, anthropic]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);
        let a = insight(&report, "p/a");

        // Uncached 500 + 200, cached 500 + 800.
        assert!((a.cache_hit_rate_pct - 65.0).abs() < 1e-9);
        assert_eq!(a.prefill_tokens_per_sec_p50, Some(200.0));
    }

    #[test]
    fn newest_three_failures_failing_and_unranked() {
        let outcomes = timed(vec![
            ok("p/a", 100, 200),
            ok("p/a", 100, 200),
            fail("p/a"),
            fail("p/a"),
            fail("p/a"),
        ]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);
        let a = insight(&report, "p/a");

        assert_eq!(a.health, ModelHealth::Failing);
        assert_eq!(a.consecutive_failures, 3);
        assert!(!a.ranked);
        assert_eq!(a.overall_score, None);
    }

    #[test]
    fn low_success_rate_degraded_but_ranked() {
        let outcomes = timed(vec![
            fail("p/a"),
            fail("p/a"),
            ok("p/a", 100, 200),
            ok("p/a", 100, 200),
        ]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);
        let a = insight(&report, "p/a");

        assert_eq!(a.health, ModelHealth::Degraded);
        assert!(a.ranked);
    }

    #[test]
    fn below_min_requests_insufficient_data_and_unranked() {
        let report = build_report(
            &timed(vec![ok("p/a", 100, 200)]),
            &HashMap::new(),
            "7d",
            5,
            NOW_MS,
        );
        let a = insight(&report, "p/a");

        assert_eq!(a.health, ModelHealth::InsufficientData);
        assert!(!a.ranked);
    }

    #[test]
    fn scores_stable_fast_model_leads() {
        let outcomes = timed(vec![
            ok("p/fast", 100, 1100),
            ok("p/fast", 100, 1100),
            ok("p/slow", 400, 4400),
            ok("p/slow", 400, 4400),
            fail("p/flaky"),
            ok("p/flaky", 100, 1100),
        ]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 2, NOW_MS);

        let fast = insight(&report, "p/fast");
        // Unknown family quality 50, 2 of 2 calls -> reliability lower bound 34.2.
        assert_eq!(fast.quality_score, Some(50.0));
        assert_eq!(fast.stability_score, Some(34.2));
        assert_eq!(fast.speed_score, Some(100.0));
        assert_eq!(fast.overall_score, Some(55.3));
        let slow = insight(&report, "p/slow");
        // decode 25/100 and ttft 100/400 -> speed 25.
        assert_eq!(slow.speed_score, Some(25.0));
        assert_eq!(slow.overall_score, Some(40.3));
        let flaky = insight(&report, "p/flaky");
        assert_eq!(flaky.stability_score, Some(9.5));
        assert_eq!(flaky.overall_score, Some(47.8));
        assert_eq!(report.models[0].model, "p/fast");
        assert_eq!(
            report.leaders.best_overall.first().map(String::as_str),
            Some("p/fast")
        );
        assert_eq!(
            report
                .leaders
                .fastest_first_token
                .first()
                .map(String::as_str),
            Some("p/fast")
        );
        assert_eq!(
            report.leaders.most_stable.first().map(String::as_str),
            Some("p/fast")
        );
    }

    #[test]
    fn pick_ttft_lowest_wins_with_alternatives() {
        let outcomes = timed(vec![
            ok("p/a", 300, 1000),
            ok("p/b", 100, 1000),
            ok("p/c", 200, 1000),
        ]);
        let report = build_report(&outcomes, &HashMap::new(), "1d", 1, NOW_MS);

        let picked = pick(&report, PickBy::Ttft).unwrap();

        assert_eq!(picked.model, "p/b");
        assert!((picked.value - 100.0).abs() < 1e-9);
        let rest: Vec<&str> = picked
            .alternatives
            .iter()
            .map(|c| c.model.as_str())
            .collect();
        assert_eq!(rest, vec!["p/c", "p/a"]);
        assert!(picked.reason.contains("1d"));
    }

    #[test]
    fn pick_without_ranked_models_none() {
        let outcomes = timed(vec![fail("p/a"), fail("p/a"), fail("p/a")]);
        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);

        assert_eq!(pick(&report, PickBy::Overall), None);
    }

    #[test]
    fn agent_report_summarises_picks_failures_and_unranked() {
        let mut outcomes = timed(vec![
            ok("p/good", 100, 1100),
            ok("p/good", 100, 1100),
            fail("p/bad"),
            fail("p/bad"),
            fail("p/bad"),
            ok("p/new", 100, 1100),
        ]);
        outcomes[3].error = None;

        let report = build_agent_report(
            &outcomes,
            &HashMap::new(),
            "7d",
            Some("p".into()),
            2,
            1_790_792_466_123,
        );

        assert_eq!(report.generated_at, "2026-09-30T18:21:06.123Z");
        assert_eq!(report.picks.len(), PickBy::ALL.len());
        assert!(report.picks.iter().all(|picked| picked.model == "p/good"));
        assert!(report.summary[0].starts_with("Best overall: p/good (100.0% success over 2 calls"));
        assert!(
            report
                .summary
                .iter()
                .any(|line| line.starts_with("p/bad is failing now: its last 3 calls failed."))
        );
        assert!(
            report
                .summary
                .iter()
                .any(|line| line == "Not ranked, fewer than 2 calls: p/new.")
        );
        let reasons: Vec<(&str, u64)> = report
            .failure_reasons
            .iter()
            .map(|reason| (reason.reason.as_str(), reason.count))
            .collect();
        assert_eq!(reasons, vec![("boom 500", 2), ("HTTP 500", 1)]);
        assert_eq!(report.failure_reasons[0].models, vec!["p/bad".to_string()]);
        assert_eq!(report.caveats.len(), CAVEATS.len());
    }

    #[test]
    fn agent_report_without_ranked_models_says_why() {
        let report = build_agent_report(
            &timed(vec![ok("p/a", 100, 200)]),
            &HashMap::new(),
            "1d",
            None,
            5,
            0,
        );

        assert!(report.picks.is_empty());
        assert!(report.summary[0].starts_with("No model has 5 or more calls in 1d"));
    }

    fn from(client: &str, mut outcome: RequestOutcome) -> RequestOutcome {
        outcome.client = Some(client.to_string());
        outcome
    }

    fn catalog(entries: &[(&str, u64, &[&str])]) -> HashMap<String, ModelCatalogEntry> {
        entries
            .iter()
            .map(|(id, ctx, uses)| {
                let entry = ModelCatalogEntry {
                    context_window: Some(*ctx),
                    use_cases: uses.iter().map(ToString::to_string).collect(),
                    summary: Some(format!("{id} summary")),
                };
                (id.to_string(), entry)
            })
            .collect()
    }

    #[test]
    fn catalog_from_models_reads_context_use_cases_and_strips_1m() {
        let payload = serde_json::json!({"data": [
            {"id": "llama_swap/qwen", "max_input_tokens": 262_144,
             "meta": {"llamaswap": {"use_cases": ["Coding", " planning "], "summary": "good coder"}}},
            {"id": "llmgw/opus[1m]", "max_input_tokens": 1_000_000},
            {"id": "llama_swap/small", "meta": {"llamaswap": {"use_cases": "fast, general"}}}
        ]});

        let parsed = catalog_from_models(&payload);

        let qwen = &parsed["llama_swap/qwen"];
        assert_eq!(qwen.context_window, Some(262_144));
        assert_eq!(qwen.use_cases, vec!["coding", "planning"]);
        assert_eq!(qwen.summary.as_deref(), Some("good coder"));
        assert_eq!(parsed["llmgw/opus"].context_window, Some(1_000_000));
        assert_eq!(
            parsed["llama_swap/small"].use_cases,
            vec!["fast", "general"]
        );
    }

    #[test]
    fn catalog_fills_fields_and_lists_unused_models() {
        let outcomes = timed(vec![ok("p/a", 100, 200)]);
        let known = catalog(&[
            ("p/a", 131_072, &["coding"]),
            ("p/idle", 32_768, &["general"]),
        ]);

        let report = build_report(&outcomes, &known, "7d", 1, NOW_MS);

        let a = insight(&report, "p/a");
        assert_eq!(a.context_window, Some(131_072));
        assert_eq!(a.use_cases, vec!["coding"]);
        let idle = insight(&report, "p/idle");
        assert_eq!(
            (idle.requests, idle.health),
            (0, ModelHealth::InsufficientData)
        );
        assert_eq!(idle.context_window, Some(32_768));
    }

    #[test]
    fn keep_warm_pings_are_not_user_requests_in_the_report() {
        let user = call("copilot/claude-haiku", 200, 50, 500);
        let mut ping = call("copilot/claude-haiku", 200, 50, 500);
        ping.client = Some(crate::consts::KEEP_WARM_CLIENT.into());
        let report = build_report(
            &[user, ping],
            &std::collections::HashMap::new(),
            "7d",
            1,
            NOW_MS,
        );
        let model = report
            .models
            .iter()
            .find(|model| model.model == "copilot/claude-haiku")
            .expect("the user call's model is reported");
        assert_eq!(model.requests, 1);
        assert_eq!(report.harnesses.iter().map(|h| h.requests).sum::<u64>(), 1);
    }

    #[test]
    fn best_harness_most_reliable_agent_harness_with_enough_calls() {
        let outcomes = timed(vec![
            from("claude-cli/2.1 (external, cli)", ok("p/a", 300, 1000)),
            from("claude-cli/2.1 (external, cli)", ok("p/a", 300, 1000)),
            from("claude-cli/2.1 (external, cli)", fail("p/a")),
            from("OpenAI/JS 5 [x-initiator]", ok("p/a", 500, 1000)),
            from("OpenAI/JS 5 [x-initiator]", ok("p/a", 500, 1000)),
            from("OpenAI/JS 5 [x-initiator]", ok("p/a", 500, 1000)),
            from("curl/8", ok("p/a", 10, 20)),
            from("curl/8", ok("p/a", 10, 20)),
            from("curl/8", ok("p/a", 10, 20)),
        ]);

        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);
        let a = insight(&report, "p/a");

        assert_eq!(a.best_harness.as_deref(), Some("copilot-cli"));
        let claude = a
            .by_harness
            .iter()
            .find(|h| h.harness == "claude-code")
            .unwrap();
        assert!((claude.success_rate_pct - 66.7).abs() < 1e-9);
        assert_eq!(report.harnesses.len(), 3);
    }

    #[test]
    fn best_for_ranked_tagged_model_wins_else_declared_fallback() {
        let outcomes = timed(vec![
            ok("p/fast", 100, 1100),
            ok("p/fast", 100, 1100),
            ok("p/slow", 400, 4400),
            ok("p/slow", 400, 4400),
        ]);
        let known = catalog(&[
            ("p/fast", 1, &["coding"]),
            ("p/slow", 1, &["coding", "planning"]),
            ("p/idle", 1, &["vision"]),
        ]);

        let report = build_report(&outcomes, &known, "7d", 2, NOW_MS);
        let by_use: HashMap<&str, &UseCasePick> = report
            .best_for
            .iter()
            .map(|pick| (pick.use_case.as_str(), pick))
            .collect();

        assert_eq!(by_use["coding"].model, "p/fast");
        assert!(by_use["coding"].ranked);
        assert!(by_use["coding"].reason.contains("all ranked models"));
        // Planning maps to a role, so the untagged faster model competes and wins.
        assert_eq!(by_use["planning"].model, "p/fast");
        assert_eq!(by_use["vision"].model, "p/idle");
        assert!(!by_use["vision"].ranked);
    }

    #[test]
    fn free_and_value_top_lists_from_priced_models() {
        let outcomes = timed(vec![
            ok("local/a", 100, 1000),
            ok("local/a", 100, 1000),
            ok("p/paid", 100, 1000),
            ok("p/paid", 100, 1000),
            ok("p/dear", 100, 1000),
            ok("p/dear", 100, 1000),
        ]);
        let mut costs = ModelCosts::new();
        costs.insert(
            "local/a".into(),
            ModelCost {
                usd: 0.0,
                free: true,
            },
        );
        costs.insert(
            "p/paid".into(),
            ModelCost {
                usd: 0.02,
                free: false,
            },
        );
        costs.insert(
            "p/dear".into(),
            ModelCost {
                usd: 0.2,
                free: false,
            },
        );

        let report = build_report_priced(&outcomes, &HashMap::new(), "7d", 1, NOW_MS, &costs);

        assert_eq!(report.best_free, vec!["local/a"]);
        assert_eq!(report.best_value, vec!["p/paid", "p/dear"]);
        let paid = insight(&report, "p/paid");
        assert_eq!(paid.cost_per_call_cents, Some(1.0));
        assert_eq!(paid.value_score, Some(55.3));
        assert_eq!(insight(&report, "p/dear").value_score, Some(5.5));
        assert!(insight(&report, "local/a").free);
        assert_eq!(insight(&report, "local/a").value_score, None);
    }

    #[test]
    fn top_lists_cap_at_three_and_use_case_top_lists_fall_back_to_calls() {
        let outcomes = timed(vec![
            ok("p/a", 100, 1000),
            ok("p/b", 100, 1000),
            ok("p/c", 100, 1000),
            ok("p/d", 100, 1000),
            ok("p/d", 100, 1000),
        ]);
        let known = catalog(&[
            ("p/a", 1, &["coding"]),
            ("p/b", 1, &["coding"]),
            ("p/c", 1, &["coding"]),
            ("p/d", 1, &["coding"]),
        ]);

        let report = build_report(&outcomes, &known, "7d", 1, NOW_MS);

        assert_eq!(report.leaders.most_used.len(), 3);
        assert_eq!(report.leaders.most_used[0], "p/d");
        // p/d has two calls, so its reliability bound leads. Equal scores tie on name.
        assert_eq!(report.best_for[0].top, vec!["p/d", "p/a", "p/b"]);
        assert!(report.best_free.is_empty());
        assert!(report.best_value.is_empty());
    }

    #[test]
    fn quality_tier_beats_a_faster_weaker_model() {
        let mut outcomes = Vec::new();
        for _ in 0..20 {
            outcomes.push(ok("local/tiny-model", 100, 1100));
            outcomes.push(ok("cloud/claude-opus-9", 400, 4400));
        }
        let report = build_report(&timed(outcomes), &HashMap::new(), "7d", 2, NOW_MS);

        assert_eq!(
            insight(&report, "cloud/claude-opus-9").quality_score,
            Some(96.7)
        );
        assert_eq!(report.leaders.best_overall[0], "cloud/claude-opus-9");
        assert_eq!(report.leaders.fastest_first_token[0], "local/tiny-model");
    }

    #[test]
    fn many_successful_calls_outrank_a_few() {
        let mut outcomes = vec![ok("p/few", 100, 1100), ok("p/few", 100, 1100)];
        outcomes.extend((0..200).map(|_| ok("p/many", 100, 1100)));
        let report = build_report(&timed(outcomes), &HashMap::new(), "7d", 2, NOW_MS);

        assert_eq!(report.leaders.most_stable[0], "p/many");
        assert_eq!(report.leaders.best_overall[0], "p/many");
    }

    #[test]
    fn speech_models_without_output_tokens_are_not_ranked() {
        let mut speech = vec![ok("stt/whisper", 50, 500), ok("stt/whisper", 50, 500)];
        for call in &mut speech {
            call.tokens_out = 0;
        }
        speech.extend([ok("p/chat", 100, 1100), ok("p/chat", 100, 1100)]);
        let report = build_report(&timed(speech), &HashMap::new(), "7d", 2, NOW_MS);

        assert!(!insight(&report, "stt/whisper").ranked);
        assert_eq!(report.leaders.best_overall, vec!["p/chat"]);
    }

    #[test]
    fn untagged_strong_model_wins_a_role_use_case_but_not_a_tagged_only_one() {
        let mut outcomes = Vec::new();
        for _ in 0..10 {
            outcomes.push(ok("local/tiny", 100, 1100));
            outcomes.push(ok("cloud/claude-sonnet-9", 300, 3300));
        }
        let known = catalog(&[
            ("local/tiny", 1, &["coding", "fast", "vision"]),
            ("cloud/claude-sonnet-9", 1, &[]),
        ]);
        let report = build_report(&timed(outcomes), &known, "7d", 2, NOW_MS);
        let by_use: HashMap<&str, &UseCasePick> = report
            .best_for
            .iter()
            .map(|pick| (pick.use_case.as_str(), pick))
            .collect();

        assert_eq!(by_use["coding"].model, "cloud/claude-sonnet-9");
        assert!(by_use["coding"].reason.contains("use-case score"));
        assert_eq!(by_use["fast"].model, "local/tiny");
        assert!(by_use["fast"].reason.contains("speed score"));
        assert_eq!(by_use["vision"].top, vec!["local/tiny"]);
        assert!(
            by_use["vision"]
                .reason
                .contains("declared for this use case")
        );
    }

    #[test]
    fn wilson_bound_handles_empty_and_full_samples() {
        assert!(wilson_lower_pct(0, 0).abs() < 1e-9);
        assert!(wilson_lower_pct(1000, 1000) > 99.0);
        assert!(wilson_lower_pct(0, 5) < 1e-9);
    }

    #[test]
    fn percentile_nearest_rank() {
        let values = [4.0, 1.0, 3.0, 2.0];
        assert_eq!(percentile(&values, MEDIAN), Some(2.0));
        assert_eq!(percentile(&values, P95), Some(4.0));
        assert_eq!(percentile(&[], MEDIAN), None);
    }
}

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
use crate::schemas::insights::model_health::ModelHealth;
use crate::schemas::insights::model_insight::ModelInsight;
use crate::schemas::insights::model_pick::ModelPick;
use crate::schemas::insights::pick_by::PickBy;
use crate::schemas::insights::pick_candidate::PickCandidate;
use crate::schemas::insights::use_case_pick::UseCasePick;
use crate::schemas::model_context::{max_input_tokens_from_item, strip_1m_suffix};
use crate::services::harness::classify;
use crate::services::stats::{RequestOutcome, tokens_per_sec};
use crate::utils::time::utc_iso8601;

/// This many newest calls failing in a row marks a model as failing.
const FAILING_STREAK: u64 = 3;
/// Below this success rate a model is degraded.
const DEGRADED_SUCCESS_PCT: f64 = 90.0;
const STABILITY_WEIGHT: f64 = 0.6;
const SPEED_WEIGHT: f64 = 0.4;
const TOP_ERRORS: usize = 3;
const PICK_ALTERNATIVES: usize = 4;
const PERCENT: f64 = 100.0;
const MEDIAN: f64 = 0.5;
const P95: f64 = 0.95;
const AGENT_FAILURE_REASONS: usize = 10;
/// A harness needs this many calls on a model before it can be its best harness.
const HARNESS_MIN_CALLS: u64 = 3;
/// Probes and scripts, not agent harnesses: never offered as a model's best harness.
const NON_AGENT_CLIENTS: [&str; 3] = ["curl", "python", "unknown"];
const MS_PER_SECOND: f64 = 1000.0;
const SCORING: &str = "overall = 0.6 x reliability (success %) + 0.4 x speed; speed = half output tok/s \
relative to the fastest ranked model + half the fastest first-token p50 relative to this model's";
const CAVEATS: [&str; 3] = [
    "Scores measure latency, throughput and failed calls through the hub, not answer quality: a model \
can be fast and healthy here and still fail agentic tasks.",
    "First-token time includes cold model loads; a single-slot local router holds one model at a time, \
so switching models costs a reload.",
    "Output speed of very short replies is dominated by overhead; compare models on similar workloads.",
];

/// Builds the insights report from outcomes in chronological order.
///
/// `catalog` adds context window and declared use cases, and lists models that
/// had no calls in the window so readers still see them.
#[must_use]
pub fn build_report(
    outcomes: &[RequestOutcome],
    catalog: &HashMap<String, ModelCatalogEntry>,
    range: &str,
    min_requests: u64,
    generated_ms: u64,
) -> InsightsReport {
    let mut by_model: HashMap<&str, Vec<&RequestOutcome>> = HashMap::new();
    for outcome in outcomes {
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
        if let Some(entry) = catalog.get(&model.model) {
            model.context_window = entry.context_window;
            model.use_cases.clone_from(&entry.use_cases);
            model.summary.clone_from(&entry.summary);
        }
    }
    score_ranked(&mut models);
    models.sort_by(report_order);
    let all_calls: Vec<&RequestOutcome> = outcomes.iter().collect();
    InsightsReport {
        range: range.to_string(),
        min_requests,
        generated_ms,
        leaders: leaders(&models),
        best_for: best_for(&models),
        harnesses: harness_insights(&all_calls),
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
#[must_use]
pub fn build_agent_report(
    outcomes: &[RequestOutcome],
    catalog: &HashMap<String, ModelCatalogEntry>,
    range: &str,
    profile: Option<String>,
    min_requests: u64,
    generated_ms: u64,
) -> AgentReport {
    let report = build_report(outcomes, catalog, range, min_requests, generated_ms);
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
    let cache_read: u64 = successes.iter().map(|call| call.cache_read_tokens).sum();

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
        cache_hit_rate_pct: ratio_pct(cache_read, tokens_in),
        tokens_in,
        tokens_out: successes.iter().map(|call| call.tokens_out).sum(),
        top_errors: top_errors(calls),
        last_success_ms: successes.last().map(|call| call.ts_ms),
        last_error_ms: calls
            .iter()
            .rev()
            .find(|call| !is_success(call))
            .map(|call| call.ts_ms),
        ranked: requests >= min_requests && health != ModelHealth::Failing,
        stability_score: None,
        speed_score: None,
        overall_score: None,
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
        stability_score: None,
        speed_score: None,
        overall_score: None,
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

/// One pick per declared use case: the best ranked model tagged with it, or,
/// when none is ranked yet, the tagged model with the most calls.
fn best_for(models: &[ModelInsight]) -> Vec<UseCasePick> {
    let use_cases: BTreeSet<&str> = models
        .iter()
        .flat_map(|model| model.use_cases.iter().map(String::as_str))
        .collect();
    use_cases
        .into_iter()
        .filter_map(|use_case| {
            let tagged: Vec<&ModelInsight> = models
                .iter()
                .filter(|model| model.use_cases.iter().any(|tag| tag == use_case))
                .collect();
            let best_ranked = tagged.iter().filter(|model| model.ranked).max_by(|a, b| {
                compare_f64(
                    a.overall_score.unwrap_or(0.0),
                    b.overall_score.unwrap_or(0.0),
                )
            });
            let (winner, ranked) = match best_ranked {
                Some(model) => (*model, true),
                None => (*tagged.iter().max_by_key(|model| model.requests)?, false),
            };
            let reason = if ranked {
                format!(
                    "highest overall score ({:.1}) among models declared for {use_case}",
                    winner.overall_score.unwrap_or(0.0)
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
                candidates: tagged.iter().map(|model| model.model.clone()).collect(),
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
        let stability = model.success_rate_pct;
        model.speed_score = Some(round_score(speed));
        model.stability_score = Some(round_score(stability));
        model.overall_score = Some(round_score(
            STABILITY_WEIGHT * stability + SPEED_WEIGHT * speed,
        ));
    }
}

fn leaders(models: &[ModelInsight]) -> InsightLeaders {
    let ranked: Vec<&ModelInsight> = models.iter().filter(|model| model.ranked).collect();
    let best = |value: fn(&ModelInsight) -> Option<f64>| {
        ranked
            .iter()
            .filter_map(|model| value(model).map(|v| (*model, v)))
            .max_by(|a, b| {
                compare_f64(a.1, b.1)
                    .then_with(|| {
                        compare_f64(
                            a.0.overall_score.unwrap_or(0.0),
                            b.0.overall_score.unwrap_or(0.0),
                        )
                    })
                    .then_with(|| a.0.requests.cmp(&b.0.requests))
            })
            .map(|(model, _)| model.model.clone())
    };
    InsightLeaders {
        best_overall: best(|model| model.overall_score),
        most_stable: best(|model| Some(model.success_rate_pct)),
        fastest_first_token: best(|model| model.ttft_p50_ms.map(|ttft| -to_f64(ttft))),
        fastest_decode: best(|model| model.decode_tokens_per_sec_p50),
        fastest_prefill: best(|model| model.prefill_tokens_per_sec_p50),
        most_used: models
            .iter()
            .max_by_key(|model| model.requests)
            .map(|model| model.model.clone()),
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
    let fresh = call.tokens_in.saturating_sub(call.cache_read_tokens);
    (fresh > 0).then(|| to_f64(fresh) * 1000.0 / to_f64(ttft))
}

fn is_success(call: &RequestOutcome) -> bool {
    call.status < ERROR_STATUS_MIN
}

/// Nearest-rank percentile; `None` for an empty sample.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // rank is within 0..=len
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

#[allow(clippy::cast_precision_loss)] // token and ms counts stay far below 2^52
fn to_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // non-negative ms values
fn round_ms(value: f64) -> u64 {
    value.round() as u64
}

fn round_score(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
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

    #[test]
    fn metrics__computed_from_successful_calls() {
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
    fn newest_three_failures__failing_and_unranked() {
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
    fn low_success_rate__degraded_but_ranked() {
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
    fn below_min_requests__insufficient_data_and_unranked() {
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
    fn scores__stable_fast_model_leads() {
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
        assert_eq!(fast.speed_score, Some(100.0));
        assert_eq!(fast.overall_score, Some(100.0));
        let slow = insight(&report, "p/slow");
        // decode 25/100 and ttft 100/400 -> speed 25.
        assert_eq!(slow.speed_score, Some(25.0));
        assert_eq!(slow.overall_score, Some(70.0));
        let flaky = insight(&report, "p/flaky");
        assert_eq!(flaky.overall_score, Some(70.0));
        assert_eq!(report.models[0].model, "p/fast");
        assert_eq!(report.leaders.best_overall.as_deref(), Some("p/fast"));
        assert_eq!(
            report.leaders.fastest_first_token.as_deref(),
            Some("p/fast")
        );
        assert_eq!(report.leaders.most_stable.as_deref(), Some("p/fast"));
    }

    #[test]
    fn pick_ttft__lowest_wins_with_alternatives() {
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
    fn pick_without_ranked_models__none() {
        let outcomes = timed(vec![fail("p/a"), fail("p/a"), fail("p/a")]);
        let report = build_report(&outcomes, &HashMap::new(), "7d", 1, NOW_MS);

        assert_eq!(pick(&report, PickBy::Overall), None);
    }

    #[test]
    fn agent_report__summarises_picks_failures_and_unranked() {
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
    fn agent_report_without_ranked_models__says_why() {
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
    fn catalog_from_models__reads_context_use_cases_and_strips_1m() {
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
    fn catalog__fills_fields_and_lists_unused_models() {
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
    fn best_harness__most_reliable_agent_harness_with_enough_calls() {
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
    fn best_for__ranked_tagged_model_wins_else_declared_fallback() {
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
        assert_eq!(by_use["planning"].model, "p/slow");
        assert_eq!(by_use["vision"].model, "p/idle");
        assert!(!by_use["vision"].ranked);
    }

    #[test]
    fn percentile__nearest_rank() {
        let values = [4.0, 1.0, 3.0, 2.0];
        assert_eq!(percentile(&values, MEDIAN), Some(2.0));
        assert_eq!(percentile(&values, P95), Some(4.0));
        assert_eq!(percentile(&[], MEDIAN), None);
    }
}

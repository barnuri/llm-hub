use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::consts::{ERROR_STATUS_MIN, PROMPT_CACHE_TTL_MS};
use crate::schemas::cache_write_cause::CacheWriteCause;
use crate::schemas::cost_parts::CostParts;
use crate::schemas::keep_warm_summary::KeepWarmSummary;
use crate::schemas::prompt_shape::PromptShape;
use crate::schemas::prompt_stats::PromptStats;
use crate::schemas::session_cache_cause::SessionCacheCause;
use crate::schemas::session_compaction::SessionCompaction;
use crate::schemas::session_composition::SessionComposition;
use crate::schemas::session_detail::SessionDetail;
use crate::schemas::session_list::SessionList;
use crate::schemas::session_prompt_point::SessionPromptPoint;
use crate::schemas::session_routed_by_spend::SessionRoutedBySpend;
use crate::schemas::session_skill_body::SessionSkillBody;
use crate::schemas::session_summary::SessionSummary;
use crate::schemas::session_tool_result::SessionToolResult;
use crate::services::harness::is_keep_warm;
use crate::services::keep_warm::{KeepWarm, WarmRequest};
use crate::services::pricing::{PricingBook, split_cached_input};
use crate::services::store::Store;

const COMPACTION_MIN_DROP_TOKENS: u64 = 10_000;
const COMPACTION_MIN_DROP_PERCENT: u64 = 30;
const SERIES_POINT_CAP: usize = 1_000;
const SESSION_LIST_LIMIT: usize = 200;
const UNKNOWN_ROUTED_BY: &str = "unknown";
const ESTIMATED_BYTES_PER_TOKEN: u64 = 4;
const SESSION_CAUSES: [CacheWriteCause; 4] = [
    CacheWriteCause::FirstInSession,
    CacheWriteCause::ModelSwitch,
    CacheWriteCause::IdleOverTtl,
    CacheWriteCause::Warm,
];

#[derive(Debug, Clone)]
struct SessionRequest {
    ts_ms: u64,
    model: String,
    status: u16,
    tokens_in: u64,
    tokens_out: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    client: Option<String>,
    worker: Option<String>,
    routed_by: Option<String>,
    shape: Option<PromptShape>,
}

/// The request stream: its shape's stream key, or its model when no key was recorded.
fn request_stream(request: &SessionRequest) -> &str {
    request
        .shape
        .as_ref()
        .and_then(|shape| shape.stream_key.as_deref())
        .unwrap_or(request.model.as_str())
}

struct Priced<'a> {
    request: &'a SessionRequest,
    prompt_tokens: u64,
    cost: CostParts,
    measured: bool,
}

struct SessionAnalysis {
    summary: SessionSummary,
    series: Vec<SessionPromptPoint>,
    compaction_pairs: Vec<(usize, usize)>,
    causes: Vec<SessionCacheCause>,
    composition: Option<SessionComposition>,
}

/// Sessions in the range, newest first, at most 200. The worker-less cache-write bucket is separate.
pub fn list_sessions(
    store: &Store,
    range: &str,
    since_ms: Option<u64>,
    book: &PricingBook,
) -> Result<SessionList, String> {
    let rows = load_requests(store, since_ms, None)?;
    let mut groups: HashMap<String, Vec<SessionRequest>> = HashMap::new();
    let mut worker_less = Vec::new();
    for row in rows {
        match row.worker.clone() {
            Some(worker) => groups.entry(worker).or_default().push(row),
            None => worker_less.push(row),
        }
    }
    let mut sessions: Vec<SessionSummary> = groups
        .iter()
        .filter_map(|(worker, rows)| analyze_session(worker, rows, book))
        .map(|analysis| analysis.summary)
        .collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_ts_ms));
    sessions.truncate(SESSION_LIST_LIMIT);
    let priced = price_successful(&worker_less, book);
    Ok(SessionList {
        range: range.to_string(),
        sessions,
        no_session_id: cache_bucket(CacheWriteCause::NoSessionId, &priced),
    })
}

/// One session with its series, compactions, cache causes and composition. `None` when the worker has no rows.
pub fn session_detail(
    store: &Store,
    worker: &str,
    since_ms: Option<u64>,
    book: &PricingBook,
) -> Result<Option<SessionDetail>, String> {
    let rows = load_requests(store, since_ms, Some(worker))?;
    let Some(analysis) = analyze_session(worker, &rows, book) else {
        return Ok(None);
    };
    let compactions = analysis
        .compaction_pairs
        .iter()
        .filter_map(|&(previous, index)| compaction_point(&analysis.series, previous, index))
        .collect();
    let compaction_at: Vec<usize> = analysis
        .compaction_pairs
        .iter()
        .map(|&(_, index)| index)
        .collect();
    let (series, series_sampled) = sample_series(analysis.series, &compaction_at);
    Ok(Some(SessionDetail {
        summary: analysis.summary,
        series,
        series_sampled,
        compactions,
        cache_causes: analysis.causes,
        composition: analysis.composition,
    }))
}

fn load_requests(
    store: &Store,
    since_ms: Option<u64>,
    worker: Option<&str>,
) -> Result<Vec<SessionRequest>, String> {
    let Store::Sqlite(conn) = store else {
        return Err("session insights need the sqlite store".into());
    };
    let guard = conn.lock().map_err(|_| "sqlite lock poisoned")?;
    let mut sql = String::from(
        "SELECT r.ts_ms, r.model, r.status, r.tokens_in, r.tokens_out, r.cache_read_tokens,
                r.cache_write_tokens, r.client, r.worker,
                s.messages, s.turn_start, s.system_chars, s.tool_count, s.tool_bytes,
                s.mcp_tool_count, s.mcp_tool_bytes, s.deferred_listing_chars,
                s.skills_listing_chars, s.claude_md_chars, s.image_count, s.project, s.request_id,
                s.stream_key, r.routed_by, s.tool_result_bytes, s.skill_body_chars
         FROM requests r LEFT JOIN request_shapes s ON s.request_id = r.id WHERE 1=1",
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(since) = since_ms {
        sql.push_str(" AND r.ts_ms >= ?");
        params.push(rusqlite::types::Value::Integer(
            i64::try_from(since).unwrap_or(i64::MAX),
        ));
    }
    if let Some(worker) = worker {
        sql.push_str(" AND r.worker = ?");
        params.push(rusqlite::types::Value::Text(worker.to_string()));
    }
    sql.push_str(" ORDER BY r.ts_ms, r.id");
    let mut stmt = guard.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), read_request)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

fn read_request(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRequest> {
    let has_shape = row.get::<_, Option<i64>>(21)?.is_some();
    let shape = has_shape.then(|| -> rusqlite::Result<PromptShape> {
        Ok(PromptShape {
            system_chars: count(row.get(11)?),
            messages: count(row.get(9)?),
            turn_start: row.get::<_, Option<i64>>(10)?.unwrap_or(0) == 1,
            tool_count: count(row.get(12)?),
            tool_bytes: count(row.get(13)?),
            mcp_tool_count: count(row.get(14)?),
            mcp_tool_bytes: count(row.get(15)?),
            deferred_listing_chars: row.get::<_, Option<i64>>(16)?.map(count),
            skills_listing_chars: row.get::<_, Option<i64>>(17)?.map(count),
            claude_md_chars: row.get::<_, Option<i64>>(18)?.map(count),
            image_count: count(row.get(19)?),
            project: row.get(20)?,
            stream_key: row.get(22)?,
            tool_result_bytes: json_counts(row.get(24)?),
            skill_body_chars: json_counts(row.get(25)?),
        })
    });
    Ok(SessionRequest {
        ts_ms: count(row.get(0)?),
        model: row.get(1)?,
        status: u16::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        tokens_in: count(row.get(3)?),
        tokens_out: count(row.get(4)?),
        cache_read_tokens: count(row.get(5)?),
        cache_write_tokens: count(row.get(6)?),
        client: row.get(7)?,
        worker: row.get(8)?,
        routed_by: row.get(23)?,
        shape: shape.transpose()?,
    })
}

fn count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// A JSON object of counts stored as text. A missing or unreadable value reads as empty.
fn json_counts(text: Option<String>) -> BTreeMap<String, u64> {
    text.and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn count_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn prompt_tokens(tokens_in: u64, cache_read: u64, cache_write: u64) -> u64 {
    let (uncached, cached) = split_cached_input(tokens_in, cache_read, cache_write);
    uncached + cached + cache_write
}

fn price_successful<'a>(rows: &'a [SessionRequest], book: &PricingBook) -> Vec<Priced<'a>> {
    rows.iter()
        .filter(|row| row.status < ERROR_STATUS_MIN)
        .map(|request| Priced {
            request,
            prompt_tokens: prompt_tokens(
                request.tokens_in,
                request.cache_read_tokens,
                request.cache_write_tokens,
            ),
            cost: book.estimate_parts(
                &request.model,
                request.tokens_in,
                request.tokens_out,
                request.cache_read_tokens,
                request.cache_write_tokens,
            ),
            measured: reports_usage(request),
        })
        .collect()
}

/// False for a row whose upstream reported no input usage (for example a cancelled stream).
fn reports_usage(request: &SessionRequest) -> bool {
    request
        .tokens_in
        .saturating_add(request.cache_read_tokens)
        .saturating_add(request.cache_write_tokens)
        > 0
}

/// One row as the keep-warm summary reads it. Only pings carry a cost; a user row carries its cache read and saving.
fn warm_request(row: &SessionRequest, book: &PricingBook) -> WarmRequest {
    let ping = is_keep_warm(row.client.as_deref());
    let ping_cost = if ping {
        book.estimate(
            &row.model,
            row.tokens_in,
            row.tokens_out,
            row.cache_read_tokens,
            row.cache_write_tokens,
        )
    } else {
        0.0
    };
    WarmRequest::new(
        row.ts_ms,
        ping,
        row.status < ERROR_STATUS_MIN,
        row.cache_read_tokens,
        ping_cost,
        book.rates_for(&row.model),
    )
}

/// Keep-warm figures for the stored requests in `since_ms` onward, summed over every session.
pub fn keep_warm_summary(
    store: &Store,
    since_ms: Option<u64>,
    book: &PricingBook,
) -> Result<KeepWarmSummary, String> {
    let rows = load_requests(store, since_ms, None)?;
    let mut by_worker: HashMap<&str, Vec<WarmRequest>> = HashMap::new();
    for row in &rows {
        if let Some(worker) = row.worker.as_deref() {
            by_worker
                .entry(worker)
                .or_default()
                .push(warm_request(row, book));
        }
    }
    let mut total = KeepWarmSummary {
        persisted: true,
        ..KeepWarmSummary::default()
    };
    for requests in by_worker.values() {
        total.absorb(&KeepWarm::summarize(requests));
    }
    Ok(total)
}

fn analyze_session(
    worker: &str,
    rows: &[SessionRequest],
    book: &PricingBook,
) -> Option<SessionAnalysis> {
    let user_rows: Vec<&SessionRequest> = rows
        .iter()
        .filter(|row| !is_keep_warm(row.client.as_deref()))
        .collect();
    let first = user_rows.first()?;
    let last = user_rows.last()?;
    let priced = price_successful(rows, book);
    let mut cost = CostParts::default();
    for point in &priced {
        cost += point.cost;
    }
    let routed_by_spend = spend_by_routed_by(&priced);
    let warm: Vec<WarmRequest> = rows.iter().map(|row| warm_request(row, book)).collect();
    let keep_warm = KeepWarmSummary {
        persisted: true,
        ..KeepWarm::summarize(&warm)
    };
    let (_pings, priced): (Vec<Priced<'_>>, Vec<Priced<'_>>) = priced
        .into_iter()
        .partition(|point| is_keep_warm(point.request.client.as_deref()));
    let measured: Vec<&Priced<'_>> = priced.iter().filter(|point| point.measured).collect();
    let series: Vec<SessionPromptPoint> = measured
        .iter()
        .map(|point| SessionPromptPoint {
            ts_ms: point.request.ts_ms,
            model: point.request.model.clone(),
            prompt_tokens: point.prompt_tokens,
            cost_usd: point.cost.total(),
        })
        .collect();
    let keyed: Vec<(&str, u64)> = measured
        .iter()
        .map(|point| (request_stream(point.request), point.prompt_tokens))
        .collect();
    let compaction_pairs = compaction_pairs(&keyed);
    let causes = session_causes(&priced);
    let idle_cache_write_usd = causes
        .iter()
        .find(|cause| cause.cause == CacheWriteCause::IdleOverTtl)
        .map_or(0.0, |cause| cause.cache_write_usd);
    let user_turns = rows
        .iter()
        .filter(|row| row.shape.as_ref().is_some_and(|shape| shape.turn_start))
        .count();
    let models: Vec<String> = user_rows
        .iter()
        .map(|row| row.model.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_string)
        .collect();
    let streams = user_rows
        .iter()
        .map(|row| request_stream(row))
        .collect::<BTreeSet<_>>()
        .len();
    let total = cost.total();
    let summary = SessionSummary {
        worker: worker.to_string(),
        client: user_rows.iter().rev().find_map(|row| row.client.clone()),
        project: user_rows
            .iter()
            .rev()
            .find_map(|row| row.shape.as_ref()?.project.clone()),
        requests: count_u64(user_rows.len()),
        user_turns: count_u64(user_turns),
        first_ts_ms: first.ts_ms,
        last_ts_ms: last.ts_ms,
        duration_ms: last.ts_ms.saturating_sub(first.ts_ms),
        models,
        streams: count_u64(streams),
        prompt: prompt_stats(&series),
        cost_usd: total,
        input_usd: cost.input,
        output_usd: cost.output,
        cache_read_usd: cost.cache_read,
        cache_write_usd: cost.cache_write,
        long_tier_usd: cost.long_tier,
        long_tier_share: if total > 0.0 {
            cost.long_tier / total
        } else {
            0.0
        },
        compactions: count_u64(compaction_pairs.len()),
        idle_cache_write_usd,
        routed_by_spend,
        keep_warm,
    };
    let composition = rows
        .iter()
        .rev()
        .find_map(|row| row.shape.as_ref())
        .map(composition_from);
    Some(SessionAnalysis {
        summary,
        series,
        compaction_pairs,
        causes,
        composition,
    })
}

/// Successful requests and cost per `routed_by`, most expensive first, ties by name.
fn spend_by_routed_by(priced: &[Priced<'_>]) -> Vec<SessionRoutedBySpend> {
    let mut groups: BTreeMap<&str, (u64, f64)> = BTreeMap::new();
    for point in priced {
        let key = point
            .request
            .routed_by
            .as_deref()
            .unwrap_or(UNKNOWN_ROUTED_BY);
        let entry = groups.entry(key).or_default();
        if !is_keep_warm(point.request.client.as_deref()) {
            entry.0 += 1;
        }
        entry.1 += point.cost.total();
    }
    let mut spend: Vec<SessionRoutedBySpend> = groups
        .into_iter()
        .map(|(routed_by, (requests, cost_usd))| SessionRoutedBySpend {
            routed_by: routed_by.to_owned(),
            requests,
            cost_usd,
        })
        .collect();
    spend.sort_by(|left, right| {
        right
            .cost_usd
            .total_cmp(&left.cost_usd)
            .then_with(|| left.routed_by.cmp(&right.routed_by))
    });
    spend
}

fn is_compaction(previous: u64, current: u64) -> bool {
    let drop = previous.saturating_sub(current);
    drop >= COMPACTION_MIN_DROP_TOKENS && drop * 100 >= previous * COMPACTION_MIN_DROP_PERCENT
}

/// Compaction drops as (previous index, current index) pairs. Each point is compared with the
/// previous point of its own stream, so interleaved subagents never look like a drop.
fn compaction_pairs(points: &[(&str, u64)]) -> Vec<(usize, usize)> {
    let mut last_of_stream: HashMap<&str, usize> = HashMap::new();
    let mut pairs = Vec::new();
    for (index, &(stream, prompt_tokens)) in points.iter().enumerate() {
        if let Some(&previous) = last_of_stream.get(stream)
            && is_compaction(points[previous].1, prompt_tokens)
        {
            pairs.push((previous, index));
        }
        last_of_stream.insert(stream, index);
    }
    pairs
}

fn compaction_point(
    series: &[SessionPromptPoint],
    previous: usize,
    index: usize,
) -> Option<SessionCompaction> {
    let previous = series.get(previous)?;
    let current = series.get(index)?;
    let drop = previous.prompt_tokens.saturating_sub(current.prompt_tokens);
    Some(SessionCompaction {
        ts_ms: current.ts_ms,
        model: current.model.clone(),
        previous_prompt_tokens: previous.prompt_tokens,
        prompt_tokens: current.prompt_tokens,
        drop_percent: drop * 100 / previous.prompt_tokens.max(1),
    })
}

fn nearest_rank(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted.get(rank - 1).copied().unwrap_or(0)
}

fn prompt_stats(series: &[SessionPromptPoint]) -> Option<PromptStats> {
    let mut sorted: Vec<u64> = series.iter().map(|point| point.prompt_tokens).collect();
    sorted.sort_unstable();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let peak = series.get(peak_index(series)?)?;
    Some(PromptStats {
        min,
        median: nearest_rank(&sorted, 50),
        p90: nearest_rank(&sorted, 90),
        max,
        peak_ts_ms: peak.ts_ms,
    })
}

fn peak_index(series: &[SessionPromptPoint]) -> Option<usize> {
    series
        .iter()
        .enumerate()
        .reduce(|best, candidate| {
            if candidate.1.prompt_tokens > best.1.prompt_tokens {
                candidate
            } else {
                best
            }
        })
        .map(|(index, _)| index)
}

fn sample_series(
    series: Vec<SessionPromptPoint>,
    compaction_at: &[usize],
) -> (Vec<SessionPromptPoint>, bool) {
    if series.len() <= SERIES_POINT_CAP {
        return (series, false);
    }
    let mut keep: BTreeSet<usize> = compaction_at.iter().copied().collect();
    keep.extend(peak_index(&series));
    let budget = SERIES_POINT_CAP.saturating_sub(keep.len());
    let total = series.len();
    if budget > 0 {
        keep.extend((0..budget).map(|step| step * total / budget));
    }
    let sampled = series
        .into_iter()
        .enumerate()
        .filter(|(index, _)| keep.contains(index))
        .map(|(_, point)| point)
        .collect();
    (sampled, true)
}

fn session_causes(priced: &[Priced<'_>]) -> Vec<SessionCacheCause> {
    let mut causes: Vec<SessionCacheCause> = SESSION_CAUSES
        .iter()
        .map(|cause| empty_cause(*cause))
        .collect();
    let mut previous_of_stream: HashMap<&str, &Priced<'_>> = HashMap::new();
    for current in priced.iter().filter(|point| point.measured) {
        let stream = request_stream(current.request);
        if current.request.cache_write_tokens > 0 {
            let cause = classify_cache_write(previous_of_stream.get(stream).copied(), current);
            if let Some(total) = causes.iter_mut().find(|total| total.cause == cause) {
                add_cache_write(total, current);
            }
        }
        previous_of_stream.insert(stream, current);
    }
    causes
}

fn classify_cache_write(previous: Option<&Priced<'_>>, current: &Priced<'_>) -> CacheWriteCause {
    let Some(previous) = previous else {
        return CacheWriteCause::FirstInSession;
    };
    if previous.request.model != current.request.model {
        CacheWriteCause::ModelSwitch
    } else if current.request.ts_ms.saturating_sub(previous.request.ts_ms) > PROMPT_CACHE_TTL_MS {
        CacheWriteCause::IdleOverTtl
    } else {
        CacheWriteCause::Warm
    }
}

fn cache_bucket(cause: CacheWriteCause, priced: &[Priced<'_>]) -> SessionCacheCause {
    let mut total = empty_cause(cause);
    for point in priced
        .iter()
        .filter(|point| point.request.cache_write_tokens > 0)
    {
        add_cache_write(&mut total, point);
    }
    total
}

fn empty_cause(cause: CacheWriteCause) -> SessionCacheCause {
    SessionCacheCause {
        cause,
        requests: 0,
        cache_write_tokens: 0,
        cache_write_usd: 0.0,
    }
}

fn add_cache_write(total: &mut SessionCacheCause, point: &Priced<'_>) {
    total.requests += 1;
    total.cache_write_tokens += point.request.cache_write_tokens;
    total.cache_write_usd += point.cost.cache_write;
}

fn estimated_tokens(size: u64) -> u64 {
    size / ESTIMATED_BYTES_PER_TOKEN
}

/// The names of a size map, largest first, ties by name.
fn largest_first(sizes: &BTreeMap<String, u64>) -> Vec<(&str, u64)> {
    let mut sorted: Vec<(&str, u64)> = sizes
        .iter()
        .map(|(name, size)| (name.as_str(), *size))
        .collect();
    sorted.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    sorted
}

fn composition_from(shape: &PromptShape) -> SessionComposition {
    SessionComposition {
        system_chars: shape.system_chars,
        system_estimated_tokens: estimated_tokens(shape.system_chars),
        tool_count: shape.tool_count,
        tool_bytes: shape.tool_bytes,
        tool_estimated_tokens: estimated_tokens(shape.tool_bytes),
        mcp_tool_count: shape.mcp_tool_count,
        mcp_tool_bytes: shape.mcp_tool_bytes,
        mcp_tool_estimated_tokens: estimated_tokens(shape.mcp_tool_bytes),
        deferred_listing_chars: shape.deferred_listing_chars,
        deferred_listing_estimated_tokens: shape.deferred_listing_chars.map(estimated_tokens),
        skills_listing_chars: shape.skills_listing_chars,
        skills_listing_estimated_tokens: shape.skills_listing_chars.map(estimated_tokens),
        claude_md_chars: shape.claude_md_chars,
        claude_md_estimated_tokens: shape.claude_md_chars.map(estimated_tokens),
        image_count: shape.image_count,
        tool_results: largest_first(&shape.tool_result_bytes)
            .into_iter()
            .map(|(family, bytes)| SessionToolResult {
                family: family.to_owned(),
                bytes,
                estimated_tokens: estimated_tokens(bytes),
            })
            .collect(),
        skill_bodies: largest_first(&shape.skill_body_chars)
            .into_iter()
            .map(|(name, chars)| SessionSkillBody {
                name: name.to_owned(),
                chars,
                estimated_tokens: estimated_tokens(chars),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;

    use super::*;
    use crate::configs::{LongContextTier, PriceCard, TokenRates};
    use crate::services::stats::RequestOutcome;

    const LONG_MODEL: &str = "copilot/demo-long-model";

    fn rates(input: f64, output: f64) -> TokenRates {
        TokenRates {
            input_per_1m: input,
            output_per_1m: output,
            cache_read_per_1m: 0.0,
            cache_write_per_1m: 0.0,
        }
    }

    fn long_tier_book() -> PricingBook {
        let card = PriceCard {
            base: rates(0.1, 0.5),
            long_context: Some(LongContextTier {
                threshold_tokens: 100_000,
                rates: rates(0.5, 2.5),
            }),
        };
        PricingBook {
            copilot_profiles: HashSet::from(["copilot".to_string()]),
            copilot_published: Arc::new(HashMap::from([("demo-long-model".to_string(), card)])),
            ..PricingBook::default()
        }
    }

    fn request(ts_ms: u64, model: &str, status: u16, tokens_in: u64) -> SessionRequest {
        SessionRequest {
            ts_ms,
            model: model.into(),
            status,
            tokens_in,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            client: Some("demo-client".into()),
            worker: Some("demo-worker".into()),
            routed_by: None,
            shape: None,
        }
    }

    fn cache_write(mut row: SessionRequest, tokens: u64) -> SessionRequest {
        row.cache_write_tokens = tokens;
        row
    }

    fn with_shape(mut row: SessionRequest, shape: PromptShape) -> SessionRequest {
        row.shape = Some(shape);
        row
    }

    fn shape(turn_start: bool, system_chars: u64) -> PromptShape {
        PromptShape {
            system_chars,
            messages: 1,
            turn_start,
            tool_count: 0,
            tool_bytes: 0,
            mcp_tool_count: 0,
            mcp_tool_bytes: 0,
            deferred_listing_chars: None,
            skills_listing_chars: None,
            claude_md_chars: None,
            image_count: 0,
            project: None,
            stream_key: None,
            tool_result_bytes: BTreeMap::new(),
            skill_body_chars: BTreeMap::new(),
        }
    }

    fn with_stream(mut row: SessionRequest, stream: &str) -> SessionRequest {
        let mut stream_shape = shape(false, 0);
        stream_shape.stream_key = Some(stream.into());
        row.shape = Some(stream_shape);
        row
    }

    fn point(ts_ms: u64, prompt_tokens: u64) -> SessionPromptPoint {
        SessionPromptPoint {
            ts_ms,
            model: "demo-model".into(),
            prompt_tokens,
            cost_usd: 0.0,
        }
    }

    fn cause_of(causes: &[SessionCacheCause], cause: CacheWriteCause) -> SessionCacheCause {
        *causes
            .iter()
            .find(|total| total.cause == cause)
            .expect("every session cause has a row")
    }

    #[test]
    fn two_requests_over_threshold_second_priced_long() {
        let book = long_tier_book();
        let rows = [
            request(1, LONG_MODEL, 200, 50_000),
            request(2, LONG_MODEL, 200, 120_000),
        ];
        let summary = analyze_session("demo-worker", &rows, &book)
            .expect("rows present")
            .summary;
        assert!((summary.long_tier_usd - 0.06).abs() < 1e-9);
        assert!((summary.cost_usd - 0.065).abs() < 1e-9);
        assert!((summary.long_tier_share - 0.06 / 0.065).abs() < 1e-9);
    }

    #[test]
    fn prompt_drop_of_forty_percent_flagged_as_compaction() {
        let keyed = [("demo-model", 100_000), ("demo-model", 60_000)];
        assert_eq!(compaction_pairs(&keyed), vec![(0, 1)]);
        let series = [point(1, 100_000), point(2, 60_000)];
        let compaction = compaction_point(&series, 0, 1).expect("both points exist");
        assert_eq!(compaction.previous_prompt_tokens, 100_000);
        assert_eq!(compaction.drop_percent, 40);
    }

    #[test]
    fn small_prompt_drop_not_a_compaction() {
        let small_absolute = [("demo-model", 20_000), ("demo-model", 15_000)];
        let small_relative = [("demo-model", 20_000), ("demo-model", 13_000)];
        assert!(compaction_pairs(&small_absolute).is_empty());
        assert!(compaction_pairs(&small_relative).is_empty());
    }

    #[test]
    fn interleaved_two_streams_give_no_compactions_or_switches() {
        let rows = [
            with_stream(request(0, "demo-model-a", 200, 100_000), "main"),
            cache_write(
                with_stream(request(1, "demo-model-b", 200, 5_000), "subagent"),
                500,
            ),
            cache_write(
                with_stream(request(2, "demo-model-a", 200, 101_000), "main"),
                500,
            ),
            cache_write(
                with_stream(request(3, "demo-model-b", 200, 5_100), "subagent"),
                500,
            ),
            with_stream(request(4, "demo-model-a", 200, 102_000), "main"),
        ];
        let analysis =
            analyze_session("demo-worker", &rows, &PricingBook::default()).expect("rows present");
        assert_eq!(analysis.summary.streams, 2);
        assert_eq!(analysis.summary.compactions, 0);
        assert_eq!(analysis.series.len(), 5);
        let causes = analysis.causes;
        assert_eq!(cause_of(&causes, CacheWriteCause::ModelSwitch).requests, 0);
        assert_eq!(cause_of(&causes, CacheWriteCause::Warm).requests, 2);
        assert_eq!(
            cause_of(&causes, CacheWriteCause::FirstInSession).requests,
            1
        );
    }

    #[test]
    fn real_switch_inside_one_stream_still_detected() {
        let rows = [
            with_stream(request(0, "demo-model-a", 200, 100_000), "main"),
            cache_write(
                with_stream(request(1, "demo-model-b", 200, 5_000), "subagent"),
                500,
            ),
            cache_write(
                with_stream(request(2, "demo-model-b", 200, 100_500), "main"),
                500,
            ),
        ];
        let analysis =
            analyze_session("demo-worker", &rows, &PricingBook::default()).expect("rows present");
        assert_eq!(
            cause_of(&analysis.causes, CacheWriteCause::ModelSwitch).requests,
            1
        );
    }

    #[test]
    fn zero_usage_row_skipped_from_series_compactions_and_causes() {
        let rows = [
            cache_write(request(0, "demo-model-a", 200, 50_000), 500),
            request(1, "demo-model-b", 200, 0),
            cache_write(request(2, "demo-model-a", 200, 50_000), 500),
        ];
        let analysis =
            analyze_session("demo-worker", &rows, &PricingBook::default()).expect("rows present");
        assert_eq!(analysis.summary.requests, 3);
        assert_eq!(analysis.series.len(), 2);
        assert_eq!(
            analysis.summary.prompt.map(|stats| stats.min),
            Some(prompt_tokens(50_000, 0, 500))
        );
        assert_eq!(analysis.summary.compactions, 0);
        assert_eq!(
            cause_of(&analysis.causes, CacheWriteCause::ModelSwitch).requests,
            0
        );
        assert_eq!(
            cause_of(&analysis.causes, CacheWriteCause::Warm).requests,
            1
        );
    }

    #[test]
    fn session_without_shape_rows_composition_is_none() {
        let book = PricingBook::default();
        let rows = [request(1, "demo-model", 200, 10)];
        let analysis = analyze_session("demo-worker", &rows, &book).expect("rows present");
        assert!(analysis.composition.is_none());
        assert_eq!(analysis.summary.user_turns, 0);
    }

    #[test]
    fn session_with_errors_errors_excluded_from_cost() {
        let book = long_tier_book();
        let mut failed = request(2, LONG_MODEL, 502, 0);
        failed.tokens_out = 999_999;
        let rows = [
            request(1, LONG_MODEL, 200, 100_000),
            failed,
            request(3, LONG_MODEL, 200, 100_000),
        ];
        let analysis = analyze_session("demo-worker", &rows, &book).expect("rows present");
        assert_eq!(analysis.summary.requests, 3);
        assert_eq!(analysis.series.len(), 2);
        assert_eq!(analysis.summary.compactions, 0);
        assert!((analysis.summary.cost_usd - 2.0 * 100_000.0 * 0.1 / 1_000_000.0).abs() < 1e-9);
        assert!(analysis.summary.long_tier_usd.abs() < 1e-12);
    }

    #[test]
    fn series_over_cap_keeps_peak_and_compactions() {
        let total = SERIES_POINT_CAP * 2 + 500;
        let series: Vec<SessionPromptPoint> = (0..total)
            .map(|index| {
                let prompt = match index {
                    1700 => 900_000,
                    _ => 50_000,
                };
                point(index as u64, prompt)
            })
            .collect();
        let keyed: Vec<(&str, u64)> = series
            .iter()
            .map(|point| ("demo-model", point.prompt_tokens))
            .collect();
        let compactions: Vec<usize> = compaction_pairs(&keyed)
            .iter()
            .map(|&(_, index)| index)
            .collect();
        assert_eq!(compactions, vec![1701]);
        let (sampled, was_sampled) =
            sample_series(series, &[compactions.as_slice(), &[1700]].concat());
        assert!(was_sampled);
        assert!(sampled.len() <= SERIES_POINT_CAP);
        assert!(sampled.iter().any(|point| point.ts_ms == 1700));
        assert!(sampled.iter().any(|point| point.ts_ms == 1701));
        assert!(sampled.windows(2).all(|pair| pair[0].ts_ms < pair[1].ts_ms));
    }

    #[test]
    fn first_request_first_in_session() {
        let rows = [cache_write(request(1, "demo-model", 200, 10), 500)];
        let priced = price_successful(&rows, &PricingBook::default());
        let causes = session_causes(&priced);
        let first = cause_of(&causes, CacheWriteCause::FirstInSession);
        assert_eq!(first.requests, 1);
        assert_eq!(first.cache_write_tokens, 500);
    }

    #[test]
    fn same_model_after_gap_over_ttl_idle_over_ttl() {
        let rows = [
            request(0, "demo-model", 200, 10),
            cache_write(request(PROMPT_CACHE_TTL_MS + 1, "demo-model", 200, 10), 500),
        ];
        let priced = price_successful(&rows, &PricingBook::default());
        let causes = session_causes(&priced);
        assert_eq!(cause_of(&causes, CacheWriteCause::IdleOverTtl).requests, 1);
        assert_eq!(cause_of(&causes, CacheWriteCause::Warm).requests, 0);
    }

    #[test]
    fn model_change_model_switch() {
        let rows = [
            with_stream(request(0, "demo-model-a", 200, 10), "main"),
            cache_write(
                with_stream(request(1_000, "demo-model-b", 200, 10), "main"),
                500,
            ),
        ];
        let priced = price_successful(&rows, &PricingBook::default());
        let causes = session_causes(&priced);
        assert_eq!(cause_of(&causes, CacheWriteCause::ModelSwitch).requests, 1);
    }

    #[test]
    fn gap_under_ttl_warm() {
        let rows = [
            request(0, "demo-model", 200, 10),
            cache_write(request(PROMPT_CACHE_TTL_MS, "demo-model", 200, 10), 500),
        ];
        let priced = price_successful(&rows, &PricingBook::default());
        let causes = session_causes(&priced);
        assert_eq!(cause_of(&causes, CacheWriteCause::Warm).requests, 1);
        assert_eq!(cause_of(&causes, CacheWriteCause::IdleOverTtl).requests, 0);
    }

    #[test]
    fn composition_from_newest_shape_uses_latest_row() {
        let rows = [
            with_shape(request(1, "demo-model", 200, 10), shape(true, 100)),
            with_shape(request(2, "demo-model", 200, 10), shape(false, 500)),
        ];
        let analysis =
            analyze_session("demo-worker", &rows, &PricingBook::default()).expect("rows present");
        let composition = analysis.composition.expect("shape rows present");
        assert_eq!(composition.system_chars, 500);
        assert_eq!(composition.system_estimated_tokens, 125);
        assert_eq!(analysis.summary.user_turns, 1);
    }

    #[test]
    fn no_worker_counted_in_no_session_bucket() {
        let mut anonymous = cache_write(request(1, "demo-model", 200, 10), 500);
        anonymous.worker = None;
        let anonymous_rows = [anonymous];
        let priced = price_successful(&anonymous_rows, &PricingBook::default());
        let bucket = cache_bucket(CacheWriteCause::NoSessionId, &priced);
        assert_eq!(bucket.requests, 1);
        assert_eq!(bucket.cache_write_tokens, 500);
    }

    #[test]
    fn sqlite_rows_join_shapes_and_group_by_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let outcome = |worker: Option<&str>, cache_write_tokens: u64| RequestOutcome {
            profile: "demo".into(),
            model_key: "demo/demo-model".into(),
            status: 200,
            ts_ms: 0,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 1_000,
            tokens_out: 10,
            cache_read_tokens: 0,
            cache_write_tokens,
            error: None,
            client: Some("demo-client".into()),
            worker: worker.map(str::to_string),
            routed_by: None,
        };
        let first = store
            .record(&outcome(Some("demo-worker"), 0))
            .unwrap()
            .unwrap();
        store.record(&outcome(Some("demo-worker"), 0)).unwrap();
        store.record(&outcome(None, 500)).unwrap();
        let mut demo_shape = shape(true, 400);
        demo_shape.project = Some("demo-project".into());
        demo_shape.stream_key = Some("demo-stream".into());
        store.record_shape(first, &demo_shape).unwrap();

        let book = PricingBook::default();
        let list = list_sessions(&store, "all", None, &book).unwrap();
        assert_eq!(list.sessions.len(), 1);
        let session = &list.sessions[0];
        assert_eq!(session.worker, "demo-worker");
        assert_eq!(session.requests, 2);
        assert_eq!(session.user_turns, 1);
        assert_eq!(session.streams, 2);
        assert_eq!(session.project.as_deref(), Some("demo-project"));
        assert_eq!(session.client.as_deref(), Some("demo-client"));
        assert_eq!(list.no_session_id.requests, 1);
        assert_eq!(list.no_session_id.cache_write_tokens, 500);

        let detail = session_detail(&store, "demo-worker", None, &book)
            .unwrap()
            .expect("worker has rows");
        assert_eq!(detail.summary.requests, 2);
        assert_eq!(detail.composition.map(|c| c.system_chars), Some(400));
        assert!(
            session_detail(&store, "missing-worker", None, &book)
                .unwrap()
                .is_none()
        );
    }

    fn routed(mut row: SessionRequest, routed_by: Option<&str>) -> SessionRequest {
        row.routed_by = routed_by.map(str::to_string);
        row
    }

    #[test]
    fn keep_warm_pings_add_cost_and_one_return_is_counted_as_cached() {
        let book = PricingBook {
            global: TokenRates {
                input_per_1m: 1.0,
                output_per_1m: 2.0,
                cache_read_per_1m: 0.1,
                cache_write_per_1m: 1.25,
            },
            ..PricingBook::default()
        };
        let mut prime = request(0, "demo-model", 200, 0);
        prime.cache_write_tokens = 5_000;
        let ping = |ts_ms: u64| {
            let mut row = request(ts_ms, "demo-model", 200, 0);
            row.client = Some(crate::consts::KEEP_WARM_CLIENT.into());
            row.cache_read_tokens = 5_000;
            row.tokens_out = 1;
            row
        };
        let mut returning = request(600_000, "demo-model", 200, 0);
        returning.cache_read_tokens = 5_000;
        let rows = [prime, ping(240_000), ping(480_000), returning];

        let analysis = analyze_session("demo-worker", &rows, &book).expect("rows present");
        let summary = &analysis.summary;
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.keep_warm.keepwarm_pings, 2);
        assert!((summary.keep_warm.keepwarm_cost_usd - 0.001_004).abs() < 1e-9);
        assert_eq!(summary.keep_warm.idle_returns_cached, 1);
        assert!((summary.keep_warm.est_write_saved_usd - 0.005_75).abs() < 1e-9);
        assert!(summary.keep_warm.persisted);
        assert!((summary.cost_usd - 0.007_754).abs() < 1e-9);
    }

    #[test]
    fn spend_by_routed_by_groups_successful_rows_and_names_unrecorded_ones() {
        let book = long_tier_book();
        let mut failed = routed(request(4, LONG_MODEL, 502, 900_000), Some("pinned"));
        failed.tokens_out = 1;
        let rows = [
            routed(
                request(1, LONG_MODEL, 200, 120_000),
                Some("smart-auto:intelligence"),
            ),
            routed(request(2, LONG_MODEL, 200, 50_000), Some("pinned")),
            routed(request(3, LONG_MODEL, 200, 50_000), None),
            failed,
        ];
        let analysis = analyze_session("demo-worker", &rows, &book).expect("rows present");
        let spend = &analysis.summary.routed_by_spend;
        let names: Vec<&str> = spend.iter().map(|row| row.routed_by.as_str()).collect();
        assert_eq!(names, ["smart-auto:intelligence", "pinned", "unknown"]);
        assert!(spend.iter().all(|row| row.requests == 1));
        let total: f64 = spend.iter().map(|row| row.cost_usd).sum();
        assert!((total - analysis.summary.cost_usd).abs() < 1e-9);
    }

    #[test]
    fn composition_lists_tool_families_and_skill_bodies_largest_first() {
        let mut newest = shape(true, 400);
        newest.tool_result_bytes = BTreeMap::from([
            ("Read".to_owned(), 4_000),
            ("mcp__files".to_owned(), 9_000),
            ("other".to_owned(), 10),
        ]);
        newest.skill_body_chars =
            BTreeMap::from([("pdf".to_owned(), 800), ("docx".to_owned(), 2_000)]);
        let rows = [with_shape(request(1, "demo-model", 200, 10), newest)];
        let analysis =
            analyze_session("demo-worker", &rows, &PricingBook::default()).expect("rows present");
        let composition = analysis.composition.expect("shape rows present");
        let families: Vec<(&str, u64, u64)> = composition
            .tool_results
            .iter()
            .map(|row| (row.family.as_str(), row.bytes, row.estimated_tokens))
            .collect();
        assert_eq!(
            families,
            [
                ("mcp__files", 9_000, 2_250),
                ("Read", 4_000, 1_000),
                ("other", 10, 2)
            ]
        );
        let skills: Vec<(&str, u64, u64)> = composition
            .skill_bodies
            .iter()
            .map(|row| (row.name.as_str(), row.chars, row.estimated_tokens))
            .collect();
        assert_eq!(skills, [("docx", 2_000, 500), ("pdf", 800, 200)]);
    }

    #[test]
    fn sqlite_routed_by_and_json_shape_columns_join_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions-json.db");
        let store = Store::open("sqlite", Some(path.to_str().unwrap())).unwrap();
        let outcome = RequestOutcome {
            profile: "demo".into(),
            model_key: "demo/demo-model".into(),
            status: 200,
            ts_ms: 0,
            latency_ms: 1,
            ttft_ms: None,
            tokens_in: 1_000,
            tokens_out: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            error: None,
            client: Some("demo-client".into()),
            worker: Some("demo-worker".into()),
            routed_by: Some("pinned".into()),
        };
        let id = store.record(&outcome).unwrap().unwrap();
        let mut demo_shape = shape(true, 400);
        demo_shape.tool_result_bytes = BTreeMap::from([("Bash".to_owned(), 77)]);
        demo_shape.skill_body_chars = BTreeMap::from([("pdf".to_owned(), 12)]);
        store.record_shape(id, &demo_shape).unwrap();

        let detail = session_detail(&store, "demo-worker", None, &PricingBook::default())
            .unwrap()
            .expect("worker has rows");
        assert_eq!(detail.summary.routed_by_spend.len(), 1);
        assert_eq!(detail.summary.routed_by_spend[0].routed_by, "pinned");
        let composition = detail.composition.expect("shape row present");
        assert_eq!(composition.tool_results[0].family, "Bash");
        assert_eq!(composition.tool_results[0].bytes, 77);
        assert_eq!(composition.skill_bodies[0].name, "pdf");
        assert_eq!(composition.skill_bodies[0].chars, 12);
    }
}

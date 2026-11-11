import { type ReactNode, useEffect, useMemo, useState } from "react";

import { CompactCount } from "../components/CompactCount";
import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { StatsTable } from "../components/StatsTable";
import { api } from "../lib/api";
import { formatMs, formatNumber, formatPct, formatTps, formatUsd } from "../lib/format";
import type { SeriesPoint, StatsOverview, StatsRange, StatsSnapshot } from "../lib/types";
import "../styles/screens/stats.css";

interface StatsScreenProps {
  readonly onError: (message: string) => void;
}

const REFRESH_MS = 5000;
const REFRESH_LABEL = `refresh ${REFRESH_MS / 1000} s`;
const HERO_SKELETONS = ["ttft", "tps", "cost", "cache", "requests"] as const;
const SPARK_DAYS = 7;
const PERCENT = 100;

const LIVE_RANGE: SegmentedOption<StatsRange> = { value: "live", label: "Live" };
const RANGES: readonly SegmentedOption<StatsRange>[] = [
  LIVE_RANGE,
  { value: "1d", label: "24 h" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "all", label: "All time" },
];

interface DayTotals {
  requests: number;
  cost: number;
  cacheRead: number;
  tokensIn: number;
}

interface DailySparks {
  readonly requests: number[];
  readonly cost: number[];
  readonly cacheHitPct: number[];
}

const METRIC_HELP: ReadonlyArray<{ readonly title: string; readonly body: string }> = [
  {
    title: "TTFT (time to first token)",
    body: "How long you wait before the model starts talking. Lower is snappier. Measured from when the hub sends the request until the first byte comes back.",
  },
  {
    title: "Tokens per second",
    body: "How fast the model keeps writing after it starts. Computed from output tokens ÷ generation time.",
  },
  {
    title: "Cache read / write",
    body: "Cache read = tokens reused from a previous similar prompt (usually cheaper). Cache write = tokens stored for later reuse.",
  },
  {
    title: "Cache hit rate",
    body: "Share of input tokens that came from cache: cache read ÷ tokens in.",
  },
  {
    title: "Latency p50 / p95",
    body: "End-to-end time for the whole reply. p50 is the typical call; p95 is the slow tail.",
  },
  {
    title: "Tokens in / out",
    body: "Tokens in = size of your prompt. Tokens out = size of the model’s answer.",
  },
  {
    title: "Cost (USD)",
    body: "Estimated spend from token counts × your rates (USD per 1M tokens). Local or unknown models stay $0 unless you set prices.",
  },
];

function emptyOverview(): StatsOverview {
  return {
    requests: 0,
    errors: 0,
    tokens_in: 0,
    tokens_out: 0,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    cache_hit_rate_pct: 0,
    p50_ms: 0,
    p95_ms: 0,
    ttft_p50_ms: 0,
    ttft_p95_ms: 0,
    tokens_per_sec_p50: 0,
    tokens_per_sec_avg: 0,
    cost_usd: 0,
    keepwarm_pings: 0,
    keepwarm_cost_usd: 0,
    idle_returns_cached: 0,
    est_write_saved_usd: 0,
    persisted: false,
  };
}

function dailySparks(snapshot: StatsSnapshot | null, range: StatsRange): DailySparks | null {
  if (!snapshot || snapshot.range !== range || snapshot.series_bucket !== "day" || !snapshot.series?.length) {
    return null;
  }
  const byDay = new Map<number, DayTotals>();
  for (const point of snapshot.series) {
    addPoint(byDay, point);
  }
  const days = [...byDay.entries()].sort(([left], [right]) => left - right).slice(-SPARK_DAYS);
  return {
    requests: days.map(([, day]) => day.requests),
    cost: days.map(([, day]) => day.cost),
    cacheHitPct: days.map(([, day]) => (day.tokensIn > 0 ? (day.cacheRead / day.tokensIn) * PERCENT : 0)),
  };
}

function addPoint(byDay: Map<number, DayTotals>, point: SeriesPoint): void {
  const day = byDay.get(point.ts_ms) ?? { requests: 0, cost: 0, cacheRead: 0, tokensIn: 0 };
  day.requests += point.requests;
  day.cost += point.cost_usd;
  day.cacheRead += point.cache_read_tokens;
  day.tokensIn += point.tokens_in;
  byDay.set(point.ts_ms, day);
}

export function StatsScreen({ onError }: StatsScreenProps) {
  const [snapshot, setSnapshot] = useState<StatsSnapshot | null>(null);
  const [range, setRange] = useState<StatsRange>("live");
  const [profile, setProfile] = useState("");
  const [model, setModel] = useState("");

  useEffect(() => {
    let cancelled = false;
    const load = () => {
      const params = new URLSearchParams({ range });
      if (profile.trim()) {
        params.set("profile", profile.trim());
      }
      if (model.trim()) {
        params.set("model", model.trim());
      }
      api<StatsSnapshot>(`/api/stats?${params}`)
        .then((body) => {
          if (!cancelled) {
            setSnapshot(body);
          }
        })
        .catch((err) => onError(err instanceof Error ? err.message : String(err)));
    };
    load();
    const timer = setInterval(load, REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [onError, range, profile, model]);

  const overview = snapshot?.overview ?? emptyOverview();
  const persistent = snapshot?.persistent ?? false;
  const filterable = snapshot?.filterable ?? false;

  const profileOptions = useMemo(() => {
    const keys = new Set<string>();
    for (const row of snapshot?.profiles ?? []) {
      keys.add(row.key);
    }
    return [...keys].sort();
  }, [snapshot]);

  const modelOptions = useMemo(() => {
    const keys = new Set<string>();
    for (const row of snapshot?.models ?? []) {
      if (!profile.trim() || row.key.startsWith(`${profile.trim()}/`)) {
        keys.add(row.key);
      }
    }
    return [...keys].sort();
  }, [snapshot, profile]);

  const rangeOptions = persistent ? RANGES : [LIVE_RANGE];
  const sparks = dailySparks(snapshot, range);

  return (
    <section className="tab active stats">
      <div className="toolbar stats-toolbar" role="group" aria-label="Stats filters">
        <Segmented label="Time range" options={rangeOptions} value={range} onChange={setRange} />
        <label className="stats-filter">
          <span>Profile</span>
          <select
            className="input"
            value={profile}
            onChange={(event) => {
              setProfile(event.target.value);
              setModel("");
            }}
          >
            <option value="">All profiles</option>
            {profileOptions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <label className="stats-filter">
          <span>Model</span>
          <select className="input" value={model} onChange={(event) => setModel(event.target.value)}>
            <option value="">All models</option>
            {modelOptions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </label>
        <span className="stats-refresh">
          <Pill tone="ok" dot title="Stats refresh on a timer">
            {REFRESH_LABEL}
          </Pill>
        </span>
      </div>

      <p className="dim stats-intro">
        {persistent ? (
          "Live counters refresh every few seconds. Day ranges use durable history from the store."
        ) : (
          <>
            Showing live counters since process start. Enable <code>LLM_HUB_PERSISTENT=true</code> for day filters
            and durable history.
          </>
        )}
        {snapshot?.pricing?.note ? ` ${snapshot.pricing.note}` : null}
      </p>

      {!filterable && range !== "live" ? (
        <p className="note">Day filters need persistence — fell back to live counters.</p>
      ) : null}

      {snapshot === null ? (
        <div className="stats-kpis" aria-busy="true" aria-label="Loading stats">
          {HERO_SKELETONS.map((id) => (
            <div key={id} className="skeleton" />
          ))}
        </div>
      ) : (
        <div className="stats-kpis">
          <KpiCard
            tone="info"
            label="TTFT (p50)"
            value={formatMs(overview.ttft_p50_ms)}
            sub="Wait before the first word appears"
          />
          <KpiCard
            label="Tokens / sec (p50)"
            value={formatTps(overview.tokens_per_sec_p50)}
            sub="Streaming speed after the reply starts"
          />
          <KpiCard
            label="Est. cost"
            value={formatUsd(overview.cost_usd)}
            sub="Estimated USD in this window"
            series={sparks?.cost}
          />
          <KpiCard
            label="Cache hit rate"
            value={formatPct(overview.cache_hit_rate_pct)}
            sub="Prompt share reused from cache"
            series={sparks?.cacheHitPct}
          />
          <KpiCard
            label="Requests"
            value={formatNumber(overview.requests)}
            sub="Total calls in this window"
            series={sparks?.requests}
          />
        </div>
      )}

      <Panel title="Details" sub="All matching calls">
        <dl className="stats-metrics">
          <Metric
            label="Errors"
            value={formatNumber(overview.errors)}
            hint="Failed calls (HTTP 400+)."
            danger={overview.errors > 0}
          />
          <Metric
            label="Cache read"
            value={<CompactCount value={overview.cache_read_tokens} />}
            hint="Input tokens served from prompt cache."
          />
          <Metric
            label="Cache write"
            value={<CompactCount value={overview.cache_write_tokens} />}
            hint="Input tokens stored into prompt cache."
          />
          <Metric label="Latency p50" value={formatMs(overview.p50_ms)} hint="Typical end-to-end time for a full reply." />
          <Metric label="Latency p95" value={formatMs(overview.p95_ms)} hint="Slow-tail end-to-end time." />
          <Metric
            label="Tokens in"
            value={<CompactCount value={overview.tokens_in} />}
            hint="Prompt size across matching calls."
          />
          <Metric
            label="Tokens out"
            value={<CompactCount value={overview.tokens_out} />}
            hint="Answer size across matching calls."
          />
          <Metric
            label="Tok/s avg"
            value={formatTps(overview.tokens_per_sec_avg)}
            hint="Average streaming speed (not just the median)."
          />
        </dl>
      </Panel>

      <StatsTable label="Profile" rows={snapshot?.profiles ?? []} />
      <StatsTable label="Model" rows={snapshot?.models ?? []} />

      <details className="panel stats-help">
        <summary>What do these numbers mean?</summary>
        <dl>
          {METRIC_HELP.map((item) => (
            <div key={item.title} className="stats-help-item">
              <dt>{item.title}</dt>
              <dd>{item.body}</dd>
            </div>
          ))}
        </dl>
      </details>
    </section>
  );
}

interface MetricProps {
  readonly label: string;
  readonly value: ReactNode;
  readonly hint: string;
  readonly danger?: boolean;
}

function Metric({ label, value, hint, danger = false }: MetricProps) {
  return (
    <div className="stats-metric" title={hint}>
      <dt>{label}</dt>
      <dd className={danger ? "t-err" : undefined}>{value}</dd>
    </div>
  );
}

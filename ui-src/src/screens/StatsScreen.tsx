import { useEffect, useMemo, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { StatsTable } from "../components/StatsTable";
import { api } from "../lib/api";
import { formatMs, formatNumber, formatPct, formatTps, formatUsd } from "../lib/format";
import type { StatsOverview, StatsRange, StatsSnapshot } from "../lib/types";
import "../styles/screens/stats.css";

interface StatsScreenProps {
  readonly onError: (message: string) => void;
}

const REFRESH_MS = 5000;
const HERO_SKELETONS = ["ttft", "tps", "cost", "cache", "requests"] as const;

const RANGES: ReadonlyArray<{ readonly id: StatsRange; readonly label: string; readonly needsPersistent: boolean }> = [
  { id: "live", label: "Live (since start)", needsPersistent: false },
  { id: "1d", label: "Last 24 hours", needsPersistent: true },
  { id: "7d", label: "Last 7 days", needsPersistent: true },
  { id: "30d", label: "Last 30 days", needsPersistent: true },
  { id: "all", label: "All time", needsPersistent: true },
];

const METRIC_HELP: ReadonlyArray<{ readonly title: string; readonly body: string }> = [
  {
    title: "TTFT (time to first token)",
    body: "How long you wait before the model starts talking. Lower is snappier. Measured from when the hub sends the request until the first byte comes back.",
  },
  {
    title: "Tokens per second",
    body: "How fast the model keeps writing after it starts. Higher means the answer streams in quicker. Computed from output tokens ÷ generation time.",
  },
  {
    title: "Cache read / write",
    body: "Prompt-cache hits and fills. Cache read = tokens reused from a previous similar prompt (usually cheaper). Cache write = tokens stored for later reuse.",
  },
  {
    title: "Cache hit rate",
    body: "Share of input tokens that came from cache: cache read ÷ tokens in. 0% means nothing was reused; higher means you are saving work.",
  },
  {
    title: "Latency p50 / p95",
    body: "End-to-end time for the whole reply. p50 is the typical call; p95 is the slow tail (95% of calls were this fast or faster).",
  },
  {
    title: "Tokens in / out",
    body: "Tokens in = size of your prompt. Tokens out = size of the model’s answer. Billing and speed usually track these.",
  },
  {
    title: "Cost (USD)",
    body: "Estimated spend from token counts × your rates (USD per 1M tokens). Cache hits usually cost less than fresh input. Local/unknown models stay $0 unless you set prices.",
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
  };
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

  return (
    <section className="tab active stats">
      <div className="stats-filters" role="group" aria-label="Stats filters">
        <div className="stats-filter">
          <label htmlFor="stats-range">Time range</label>
          <select
            id="stats-range"
            className="input"
            value={range}
            onChange={(event) => setRange(event.target.value as StatsRange)}
          >
            {RANGES.map((option) => (
              <option key={option.id} value={option.id} disabled={option.needsPersistent && !persistent}>
                {option.label}
                {option.needsPersistent && !persistent ? " (needs persistence)" : ""}
              </option>
            ))}
          </select>
        </div>

        <div className="stats-filter">
          <label htmlFor="stats-profile">Profile</label>
          <select
            id="stats-profile"
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
        </div>

        <div className="stats-filter">
          <label htmlFor="stats-model">Model</label>
          <select
            id="stats-model"
            className="input"
            value={model}
            onChange={(event) => setModel(event.target.value)}
          >
            <option value="">All models</option>
            {modelOptions.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>

        <p className="note stats-filters-note">
          {persistent ? (
            "Live counters refresh every few seconds. Day ranges use durable history from the store."
          ) : (
            <>
              Showing live counters since process start. Enable <code>LLM_HUB_PERSISTENT=true</code> for day filters
              and durable history.
            </>
          )}
        </p>
      </div>

      {!filterable && range !== "live" ? (
        <p className="note">Day filters need persistence — fell back to live counters.</p>
      ) : null}

      {snapshot?.pricing?.note ? <p className="note">{snapshot.pricing.note}</p> : null}

      {snapshot === null ? (
        <div className="kpis" aria-busy="true" aria-label="Loading stats">
          {HERO_SKELETONS.map((id) => (
            <div key={id} className="skeleton" />
          ))}
        </div>
      ) : (
        <div className="kpis">
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
          <KpiCard label="Est. cost" value={formatUsd(overview.cost_usd)} sub="Estimated USD in this window" />
          <KpiCard
            label="Cache hit rate"
            value={formatPct(overview.cache_hit_rate_pct)}
            sub="Prompt share reused from cache"
          />
          <KpiCard label="Requests" value={formatNumber(overview.requests)} sub="Total calls in this window" />
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
          <Metric label="Cache read" value={formatNumber(overview.cache_read_tokens)} hint="Input tokens served from prompt cache." />
          <Metric label="Cache write" value={formatNumber(overview.cache_write_tokens)} hint="Input tokens stored into prompt cache." />
          <Metric label="Latency p50" value={formatMs(overview.p50_ms)} hint="Typical end-to-end time for a full reply." />
          <Metric label="Latency p95" value={formatMs(overview.p95_ms)} hint="Slow-tail end-to-end time." />
          <Metric label="Tokens in" value={formatNumber(overview.tokens_in)} hint="Prompt size across matching calls." />
          <Metric label="Tokens out" value={formatNumber(overview.tokens_out)} hint="Answer size across matching calls." />
          <Metric label="Tok/s avg" value={formatTps(overview.tokens_per_sec_avg)} hint="Average streaming speed (not just the median)." />
        </dl>
      </Panel>

      <StatsTable label="Profile" rows={snapshot?.profiles ?? []} />
      <StatsTable label="Model" rows={snapshot?.models ?? []} />

      <details className="stats-help">
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
  readonly value: string;
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

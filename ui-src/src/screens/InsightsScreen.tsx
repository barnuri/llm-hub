import { useEffect, useState } from "react";

import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { api } from "../lib/api";
import { formatMs, formatNumber, formatPct, formatTps } from "../lib/format";
import { navigate, useRoute } from "../lib/router";
import type {
  HarnessInsight,
  InsightsRange,
  InsightsReport,
  ModelHealth,
  ModelInsight,
  ModelPick,
  UseCasePick,
} from "../lib/types";
import "../styles/screens/insights.css";

interface InsightsScreenProps {
  readonly onCopy: (text: string) => void;
}

const RANGES: readonly SegmentedOption<InsightsRange>[] = [
  { value: "1d", label: "Today" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "all", label: "All time" },
];

const MIN_CALL_OPTIONS: readonly number[] = [1, 3, 5, 10, 25];
const DEFAULT_MIN_CALLS = 5;
const STABILITY_WEIGHT = 0.6;
const SPEED_WEIGHT = 0.4;

const HEALTH_LABEL: Readonly<Record<ModelHealth, string>> = {
  healthy: "Healthy",
  degraded: "Degraded",
  failing: "Failing now",
  insufficient_data: "Too few calls",
};

const HEALTH_TONE: Readonly<Record<ModelHealth, PillTone>> = {
  healthy: "ok",
  degraded: "warn",
  failing: "danger",
  insufficient_data: "neutral",
};

function parseRange(value: string | null): InsightsRange {
  if (value === "1d" || value === "7d" || value === "30d" || value === "all") {
    return value;
  }
  return "7d";
}

function parseMinCalls(value: string | null): number {
  const parsed = Number(value);
  return MIN_CALL_OPTIONS.includes(parsed) ? parsed : DEFAULT_MIN_CALLS;
}

function shortName(model: string | null): string {
  if (!model) {
    return "—";
  }
  const slash = model.indexOf("/");
  return slash >= 0 ? model.slice(slash + 1) : model;
}

function formatContext(tokens: number | null): string {
  if (!tokens) {
    return "—";
  }
  if (tokens >= 1_000_000) {
    return `${formatNumber(tokens / 1_000_000, 1)}M`;
  }
  const unit = tokens % 1024 === 0 ? 1024 : 1000;
  return `${formatNumber(Math.round(tokens / unit))}k`;
}

function queryString(range: InsightsRange, minCalls: number): string {
  return `range=${range}&min_requests=${minCalls}`;
}

export function InsightsScreen({ onCopy }: InsightsScreenProps) {
  const route = useRoute();
  const range = parseRange(route.query.get("range"));
  const minCalls = parseMinCalls(route.query.get("min"));
  const [report, setReport] = useState<InsightsReport | null>(null);
  const [pick, setPick] = useState<ModelPick | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const query = queryString(range, minCalls);
    setUnavailable(null);
    api<InsightsReport>(`/api/insights?${query}`)
      .then((body) => {
        if (!cancelled) {
          setReport(body);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setReport(null);
          setUnavailable(err instanceof Error ? err.message : String(err));
        }
      });
    api<ModelPick>(`/api/insights/pick?${query}`)
      .then((body) => {
        if (!cancelled) {
          setPick(body);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setPick(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [range, minCalls]);

  const go = (nextRange: InsightsRange, nextMin: number) =>
    navigate(`/insights?range=${nextRange}&min=${nextMin}`);

  const models = report?.models ?? [];
  const ranked = models.filter((model) => model.ranked);
  const unranked = models.filter((model) => !model.ranked);
  const pickUrl = `${window.location.origin}/api/insights/pick?${queryString(range, minCalls)}`;
  const agentReportPath = `/api/insights/report?${queryString(range, minCalls)}`;

  return (
    <section className="tab active insights" aria-busy={report === null && !unavailable}>
      <div className="toolbar insights-toolbar">
        <Segmented label="Insights period" options={RANGES} value={range} onChange={(value) => go(value, minCalls)} />
        <label className="insights-min">
          <span>Rank models with at least</span>
          <select
            className="input"
            value={minCalls}
            onChange={(event) => go(range, Number(event.target.value))}
          >
            {MIN_CALL_OPTIONS.map((option) => (
              <option key={option} value={option}>
                {option} {option === 1 ? "call" : "calls"}
              </option>
            ))}
          </select>
        </label>
      </div>
      <p className="dim insights-intro">
        Which model has been fastest and most reliable, from real calls through the hub. These numbers measure speed
        and failed calls, not answer quality.
      </p>

      {unavailable ? (
        <Panel title="Insights unavailable">
          <p className="note" role="status">
            {unavailable}
          </p>
        </Panel>
      ) : null}

      <div className="grid-main-side insights-top">
        {pick ? (
          <Panel
            title="Recommended model"
            className="insights-pick"
            actions={
              <button type="button" className="btn small" onClick={() => onCopy(pickUrl)}>
                Copy pick URL
              </button>
            }
          >
            <div role="status">
              <div className="insights-pick-title">
                Use <span className="mono">{pick.model}</span>
              </div>
              <div className="dim">{pick.reason}</div>
              {pick.alternatives.length > 0 ? (
                <div className="dim">
                  Next best:{" "}
                  <span className="mono">{pick.alternatives.map((alt) => shortName(alt.model)).join(", ")}</span>
                </div>
              ) : null}
            </div>
          </Panel>
        ) : (
          <Panel title="Recommended model" className="insights-pick">
            <p className="note" role="status">
              {report && !unavailable
                ? `No model has ${minCalls} or more calls in this window yet. Lower the minimum or pick a longer range.`
                : "Loading…"}
            </p>
          </Panel>
        )}
        <Panel title="Agent report" sub="JSON for agents and scripts">
          <p className="dim insights-agent-text">One JSON with a summary, every pick and failure reasons.</p>
          <div className="toolbar">
            <a className="btn small" href={agentReportPath} target="_blank" rel="noreferrer">
              Open agent report
            </a>
            <button
              type="button"
              className="btn small"
              onClick={() => onCopy(`${window.location.origin}${agentReportPath}`)}
            >
              Copy agent report URL
            </button>
          </div>
        </Panel>
      </div>

      {report ? <Leaders report={report} /> : null}

      {report && report.best_for.length > 0 ? <BestFor picks={report.best_for} /> : null}

      {ranked.length > 0 ? (
        <Panel
          title="Ranking"
          sub="Overall score = 60% reliability + 40% speed"
          actions={
            <div className="insights-legend dim" aria-hidden="true">
              <span className="insights-swatch reliability" /> Reliability
              <span className="insights-swatch speed" /> Speed
            </div>
          }
        >
          <p className="dim insights-panel-note">
            Speed compares first-token time and output speed with the best ranked model.
          </p>
          <ol className="insights-ranking">
            {ranked.map((model) => (
              <RankingRow key={model.model} model={model} />
            ))}
          </ol>
        </Panel>
      ) : null}

      {models.length > 0 ? (
        <Panel title="All models" sub={`${formatNumber(models.length)} models, ranked first`}>
          <div className="table-scroll insights-table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Model</th>
                  <th>Good for</th>
                  <th>Health</th>
                  <th className="num">Context</th>
                  <th className="num">Calls</th>
                  <th className="num">Success</th>
                  <th className="num">First token p50</th>
                  <th className="num">First token p95</th>
                  <th className="num">Output speed</th>
                  <th className="num">Prompt reading</th>
                  <th className="num">Cache hit</th>
                  <th>Best harness</th>
                  <th>Most common error</th>
                </tr>
              </thead>
              <tbody>
                {[...ranked, ...unranked].map((model) => (
                  <DetailRow key={model.model} model={model} />
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      ) : report && !unavailable ? (
        <Panel title="All models">
          <p className="note insights-empty">No calls in this window yet.</p>
        </Panel>
      ) : null}

      {report && report.harnesses.length > 0 ? <Harnesses harnesses={report.harnesses} /> : null}
    </section>
  );
}

function BestFor({ picks }: { readonly picks: readonly UseCasePick[] }) {
  return (
    <Panel title="Best model for each job" sub="Best-ranked model declared for each use case in the registry">
      <div className="kpis insights-cards">
        {picks.map((pick) => (
          <div key={pick.use_case} className="kpi insights-card" title={pick.reason}>
            <span className="kpi-label">Best for {pick.use_case}</span>
            <span className="insights-card-model" title={pick.model ?? undefined}>
              {shortName(pick.model)}
            </span>
            <span className="kpi-sub">{pick.ranked ? "Ranked from calls" : "Declared only, not enough calls yet"}</span>
          </div>
        ))}
      </div>
    </Panel>
  );
}

function Harnesses({ harnesses }: { readonly harnesses: readonly HarnessInsight[] }) {
  return (
    <Panel title="By harness" sub="Which client sent the calls, told apart by its User-Agent and headers">
      <div className="table-scroll insights-table-scroll">
        <table className="table insights-harness-table">
          <thead>
            <tr>
              <th>Harness</th>
              <th className="num">Calls</th>
              <th className="num">Success</th>
              <th className="num">First token p50</th>
              <th className="num">Output speed</th>
            </tr>
          </thead>
          <tbody>
            {harnesses.map((harness) => (
              <tr key={harness.harness}>
                <td className="mono">{harness.harness}</td>
                <td className="num">{formatNumber(harness.requests)}</td>
                <td className="num">{formatPct(harness.success_rate_pct)}</td>
                <td className="num nowrap">{formatMs(harness.ttft_p50_ms ?? 0)}</td>
                <td className="num nowrap">{formatTps(harness.decode_tokens_per_sec_p50 ?? 0)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}

function Leaders({ report }: { readonly report: InsightsReport }) {
  const byId = new Map(report.models.map((model) => [model.model, model]));
  const tiles: ReadonlyArray<readonly [string, string | null, string]> = [
    ["Most reliable", report.leaders.most_stable, formatPct(byId.get(report.leaders.most_stable ?? "")?.success_rate_pct ?? NaN)],
    [
      "Quickest first token",
      report.leaders.fastest_first_token,
      formatMs(byId.get(report.leaders.fastest_first_token ?? "")?.ttft_p50_ms ?? 0),
    ],
    [
      "Fastest output",
      report.leaders.fastest_decode,
      formatTps(byId.get(report.leaders.fastest_decode ?? "")?.decode_tokens_per_sec_p50 ?? 0),
    ],
    [
      "Fastest prompt reading",
      report.leaders.fastest_prefill,
      formatTps(byId.get(report.leaders.fastest_prefill ?? "")?.prefill_tokens_per_sec_p50 ?? 0),
    ],
    [
      "Most used",
      report.leaders.most_used,
      `${formatNumber(byId.get(report.leaders.most_used ?? "")?.requests ?? 0)} calls`,
    ],
  ];
  return (
    <div className="kpis insights-leaders">
      {tiles.map(([label, model, value]) => (
        <div key={label} className="kpi">
          <span className="kpi-label">{label}</span>
          <span className="kpi-value">{model ? value : "—"}</span>
          <span className="kpi-sub insights-leader" title={model ?? undefined}>
            {model ? shortName(model) : "No ranked model yet"}
          </span>
        </div>
      ))}
    </div>
  );
}

function RankingRow({ model }: { readonly model: ModelInsight }) {
  const reliability = STABILITY_WEIGHT * (model.stability_score ?? 0);
  const speed = SPEED_WEIGHT * (model.speed_score ?? 0);
  return (
    <li className="insights-rank-row">
      <span className="insights-rank-name mono" title={model.model}>
        {shortName(model.model)}
      </span>
      <HealthPill health={model.health} />
      <span
        className="insights-bar"
        role="img"
        aria-label={`Overall ${formatNumber(model.overall_score ?? 0, 1)}: reliability ${formatNumber(reliability, 1)}, speed ${formatNumber(speed, 1)}`}
      >
        <span className="insights-bar-part reliability" style={{ width: `${reliability}%` }} />
        <span className="insights-bar-part speed" style={{ width: `${speed}%` }} />
      </span>
      <span className="insights-score mono">{formatNumber(model.overall_score ?? 0, 1)}</span>
    </li>
  );
}

function DetailRow({ model }: { readonly model: ModelInsight }) {
  const topError = model.top_errors[0];
  return (
    <tr>
      <td className="mono nowrap" title={model.summary ?? model.model}>
        {shortName(model.model)}
      </td>
      <td className="nowrap">
        {model.use_cases.length > 0 ? (
          model.use_cases.map((useCase) => (
            <span key={useCase} className="insights-use">
              <Pill tone="info">{useCase}</Pill>
            </span>
          ))
        ) : (
          <span className="dim">—</span>
        )}
      </td>
      <td>
        <HealthPill health={model.health} />
      </td>
      <td className="num">{formatContext(model.context_window)}</td>
      <td className="num">{formatNumber(model.requests)}</td>
      <td className="num">{formatPct(model.success_rate_pct)}</td>
      <td className="num nowrap">{formatMs(model.ttft_p50_ms ?? 0)}</td>
      <td className="num nowrap">{formatMs(model.ttft_p95_ms ?? 0)}</td>
      <td className="num nowrap">{formatTps(model.decode_tokens_per_sec_p50 ?? 0)}</td>
      <td className="num nowrap">{formatTps(model.prefill_tokens_per_sec_p50 ?? 0)}</td>
      <td className="num">{formatPct(model.cache_hit_rate_pct)}</td>
      <td className="mono nowrap">{model.best_harness ?? <span className="dim">—</span>}</td>
      <td className="error-reason" title={topError?.reason}>
        {topError ? `${topError.reason} (×${topError.count})` : <span className="dim">None</span>}
      </td>
    </tr>
  );
}

function HealthPill({ health }: { readonly health: ModelHealth }) {
  return <Pill tone={HEALTH_TONE[health]}>{HEALTH_LABEL[health]}</Pill>;
}

import { useEffect, useState } from "react";

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

interface InsightsScreenProps {
  readonly onCopy: (text: string) => void;
}

const RANGES: ReadonlyArray<readonly [InsightsRange, string]> = [
  ["1d", "Today"],
  ["7d", "Last 7 days"],
  ["30d", "Last 30 days"],
  ["all", "All time"],
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
    <section className="tab active insights">
      <h2>Insights</h2>
      <p className="dim">
        Which model has been fastest and most reliable, from real calls through the hub. These numbers measure
        speed and failed calls, not answer quality.
      </p>

      <div className="toolbar">
        {RANGES.map(([id, label]) => (
          <button
            key={id}
            type="button"
            className={range === id ? "chip active" : "chip"}
            onClick={() => go(id, minCalls)}
          >
            {label}
          </button>
        ))}
        <label className="insights-min">
          <span className="dim">Rank models with at least</span>
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

      <div className="insights-agent">
        <span className="dim">For agents and scripts: one JSON with a summary, every pick and failure reasons.</span>
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

      {unavailable ? (
        <p className="note" role="status">
          {unavailable}
        </p>
      ) : null}

      {pick ? (
        <div className="insights-pick" role="status">
          <div>
            <div className="insights-pick-title">
              Use <span className="mono">{pick.model}</span>
            </div>
            <div className="dim">{pick.reason}</div>
            {pick.alternatives.length > 0 ? (
              <div className="dim">
                Next best: {pick.alternatives.map((alt) => shortName(alt.model)).join(", ")}
              </div>
            ) : null}
          </div>
          <button type="button" className="btn small" onClick={() => onCopy(pickUrl)}>
            Copy pick URL
          </button>
        </div>
      ) : report && !unavailable ? (
        <p className="note" role="status">
          No model has {minCalls} or more calls in this window yet. Lower the minimum or pick a longer range.
        </p>
      ) : null}

      {report ? <Leaders report={report} /> : null}

      {report && report.best_for.length > 0 ? <BestFor picks={report.best_for} /> : null}

      {ranked.length > 0 ? (
        <>
          <h3>Ranking</h3>
          <p className="dim">
            Overall score = 60% reliability + 40% speed. Speed compares first-token time and output speed with the
            best ranked model.
          </p>
          <ol className="insights-ranking">
            {ranked.map((model) => (
              <RankingRow key={model.model} model={model} />
            ))}
          </ol>
          <div className="insights-legend dim" aria-hidden="true">
            <span className="insights-swatch reliability" /> Reliability share
            <span className="insights-swatch speed" /> Speed share
          </div>
        </>
      ) : null}

      {models.length > 0 ? (
        <>
          <h3>All models</h3>
          <div className="table-scroll">
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
        </>
      ) : null}

      {report && report.harnesses.length > 0 ? <Harnesses harnesses={report.harnesses} /> : null}
    </section>
  );
}

function BestFor({ picks }: { readonly picks: readonly UseCasePick[] }) {
  return (
    <>
      <h3>Best model for each job</h3>
      <p className="dim">
        Use cases come from the model registry; the pick is the best-ranked model declared for each one.
      </p>
      <div className="stat-tiles">
        {picks.map((pick) => (
          <div key={pick.use_case} className="stat-tile" title={pick.reason}>
            <div className="label">Best for {pick.use_case}</div>
            <div className="insights-leader mono">{shortName(pick.model)}</div>
            <div className="hint">{pick.ranked ? "Ranked from calls" : "Declared only, not enough calls yet"}</div>
          </div>
        ))}
      </div>
    </>
  );
}

function Harnesses({ harnesses }: { readonly harnesses: readonly HarnessInsight[] }) {
  return (
    <>
      <h3>By harness</h3>
      <p className="dim">Which client sent the calls, told apart by its User-Agent and headers.</p>
      <div className="table-scroll">
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
    </>
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
    <div className="stat-tiles">
      {tiles.map(([label, model, value]) => (
        <div key={label} className="stat-tile">
          <div className="label">{label}</div>
          <div className="insights-leader mono" title={model ?? undefined}>
            {shortName(model)}
          </div>
          <div className="hint">{model ? value : "No ranked model yet"}</div>
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
              {useCase}
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
  return <span className={`insights-health ${health}`}>{HEALTH_LABEL[health]}</span>;
}

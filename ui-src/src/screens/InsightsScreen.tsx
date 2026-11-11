import { useEffect, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { ModelName } from "../components/ModelName";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { ShareBar } from "../components/ShareBar";
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
const MIN_CALL_SEGMENTS: readonly SegmentedOption<string>[] = MIN_CALL_OPTIONS.map((option) => ({
  value: String(option),
  label: String(option),
}));
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

function fullName(model: string | null): string {
  return model || "—";
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

function formatRate(tokensPerSec: number | null): string {
  if (!tokensPerSec) {
    return "—";
  }
  return formatNumber(tokensPerSec, tokensPerSec >= 100 ? 0 : 1);
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
  const agentReportUrl = `${window.location.origin}${agentReportPath}`;
  const pickHealth = pick ? models.find((model) => model.model === pick.model)?.health : undefined;

  return (
    <section className="tab active insights" aria-busy={report === null && !unavailable}>
      <div className="toolbar insights-toolbar">
        <Segmented label="Insights period" options={RANGES} value={range} onChange={(value) => go(value, minCalls)} />
        <div className="insights-min">
          <span aria-hidden="true">Rank models with at least</span>
          <Segmented
            label="Minimum calls to rank a model"
            options={MIN_CALL_SEGMENTS}
            value={String(minCalls)}
            onChange={(value) => go(range, Number(value))}
          />
          <span aria-hidden="true">calls</span>
        </div>
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
            <div role="status" className="insights-pick-body">
              <div className="insights-pick-title">
                <span>Use</span>
                <span className="mono insights-pick-model"><ModelName id={pick.model} /></span>
                {pickHealth ? <HealthPill health={pickHealth} /> : null}
              </div>
              <div className="insights-pick-reason">{pick.reason}</div>
              {pick.alternatives.length > 0 ? (
                <div className="dim insights-pick-next">
                  Next best:{" "}
                  <span className="mono">{pick.alternatives.map((alt) => fullName(alt.model)).join(", ")}</span>
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
          <p className="insights-agent-text">One JSON with a summary, every pick and failure reasons.</p>
          <div className="mono insights-agent-url">{agentReportUrl.replace(/^https?:\/\//, "")}</div>
          <div className="toolbar">
            <a className="btn small" href={agentReportPath} target="_blank" rel="noreferrer">
              Open agent report
            </a>
            <button
              type="button"
              className="btn small"
              onClick={() => onCopy(agentReportUrl)}
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
          sub="overall score = 60% reliability + 40% speed"
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
          <div className="table-scroll">
            <table className="table insights-ranking">
              <thead>
                <tr>
                  <th className="num">#</th>
                  <th>Model</th>
                  <th>Health</th>
                  <th className="num">Calls</th>
                  <th className="num">Success</th>
                  <th className="insights-score-col">Score</th>
                  <th className="num">Overall</th>
                </tr>
              </thead>
              <tbody>
                {ranked.map((model, index) => (
                  <RankingRow key={model.model} model={model} position={index + 1} />
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      ) : null}

      {models.length > 0 ? (
        <Panel
          title="All models"
          sub={`${formatNumber(models.length)} models, ranked first`}
          actions={
            <span className="insights-abbr">TTFT = first token · Out = output speed · Prefill = prompt reading</span>
          }
        >
          <div className="table-scroll insights-table-scroll">
            <table className="table insights-models">
              <thead>
                <tr>
                  <th>Model</th>
                  <th>Good for</th>
                  <th>Health</th>
                  <th className="num" title="Context window">
                    Ctx
                  </th>
                  <th className="num">Calls</th>
                  <th className="num">Success</th>
                  <th className="num" title="Time to first token, median">
                    TTFT p50
                  </th>
                  <th className="num" title="Time to first token, slow tail">
                    TTFT p95
                  </th>
                  <th className="num" title="Output speed, median">
                    Out tok/s
                  </th>
                  <th className="num" title="Prompt reading speed, median">
                    Prefill tok/s
                  </th>
                  <th className="num" title="Prompt share served from cache">
                    Cache
                  </th>
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
    <Panel title="Best model for each job" sub="best-ranked model declared for each use case in the registry">
      <div className="insights-strip insights-cards">
        {picks.map((pick) => (
          <div key={pick.use_case} className="kpi insights-card" title={pick.reason}>
            <span className="kpi-label">Best for {pick.use_case}</span>
            <span
              className={pick.ranked ? "insights-card-model" : "insights-card-model declared"}
              title={pick.model ?? undefined}
            >
              {fullName(pick.model)}
            </span>
            <span className="kpi-sub">{pick.ranked ? "Ranked from calls" : "Declared only, not enough calls yet"}</span>
          </div>
        ))}
      </div>
    </Panel>
  );
}

function Harnesses({ harnesses }: { readonly harnesses: readonly HarnessInsight[] }) {
  const totalCalls = harnesses.reduce((sum, harness) => sum + harness.requests, 0);
  return (
    <Panel title="By harness" sub="which client sent the calls, told apart by its User-Agent and headers">
      <div className="table-scroll insights-table-scroll">
        <table className="table insights-harness-table">
          <thead>
            <tr>
              <th>Harness</th>
              <th className="num">Calls</th>
              <th className="insights-share-col">Share</th>
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
                <td>
                  <ShareBar
                    fraction={totalCalls > 0 ? harness.requests / totalCalls : 0}
                    label={`${formatPct(totalCalls > 0 ? (harness.requests / totalCalls) * 100 : 0)} of calls`}
                  />
                </td>
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
    <div className="insights-strip insights-leaders">
      {tiles.map(([label, model, value]) => (
        <KpiCard
          key={label}
          label={label}
          value={model ? value : "—"}
          sub={model ? fullName(model) : "No ranked model yet"}
        />
      ))}
    </div>
  );
}

function RankingRow({ model, position }: { readonly model: ModelInsight; readonly position: number }) {
  const reliability = STABILITY_WEIGHT * (model.stability_score ?? 0);
  const speed = SPEED_WEIGHT * (model.speed_score ?? 0);
  return (
    <tr>
      <td className="num insights-rank">{position}</td>
      <td className="mono nowrap insights-rank-name">
        <ModelName id={model.model} />
      </td>
      <td>
        <HealthPill health={model.health} />
      </td>
      <td className="num">{formatNumber(model.requests)}</td>
      <td className="num">{formatPct(model.success_rate_pct)}</td>
      <td>
        <span
          className="insights-bar"
          role="img"
          aria-label={`Overall ${formatNumber(model.overall_score ?? 0, 1)}: reliability ${formatNumber(reliability, 1)}, speed ${formatNumber(speed, 1)}`}
        >
          <span className="insights-bar-part reliability" style={{ width: `${reliability}%` }} />
          <span className="insights-bar-part speed" style={{ width: `${speed}%` }} />
        </span>
      </td>
      <td className="num insights-score">{formatNumber(model.overall_score ?? 0, 1)}</td>
    </tr>
  );
}

function DetailRow({ model }: { readonly model: ModelInsight }) {
  const topError = model.top_errors[0];
  return (
    <tr>
      <td
        className={model.ranked ? "mono nowrap" : "mono nowrap insights-unranked"}
        title={model.summary ?? model.model}
      >
        {fullName(model.model)}
      </td>
      <td className="nowrap">
        {model.use_cases.length > 0 ? (
          model.use_cases.map((useCase) => (
            <span key={useCase} className="insights-use">
              <Pill>{useCase}</Pill>
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
      <td className="num nowrap">{formatRate(model.decode_tokens_per_sec_p50)}</td>
      <td className="num nowrap">{formatRate(model.prefill_tokens_per_sec_p50)}</td>
      <td className="num">{formatPct(model.cache_hit_rate_pct)}</td>
      <td className="mono nowrap">{model.best_harness ?? <span className="dim">—</span>}</td>
      <td className={topError ? "mono insights-top-error" : undefined} title={topError?.reason}>
        {topError ? `${topError.reason} (×${topError.count})` : <span className="dim">None</span>}
      </td>
    </tr>
  );
}

function HealthPill({ health }: { readonly health: ModelHealth }) {
  return <Pill tone={HEALTH_TONE[health]}>{HEALTH_LABEL[health]}</Pill>;
}

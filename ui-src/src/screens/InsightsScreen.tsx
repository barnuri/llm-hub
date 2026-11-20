import { useEffect, useState } from "react";

import { ModelName } from "../components/ModelName";
import { Panel } from "../components/Panel";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { ShareBar } from "../components/ShareBar";
import { TopList, type TopListItem } from "../components/TopList";
import { api } from "../lib/api";
import { formatCompact, formatMs, formatNumber, formatPct, formatTps, formatUsd } from "../lib/format";
import { navigate, useRoute } from "../lib/router";
import type {
  HarnessInsight,
  InsightLeaders,
  InsightsRange,
  InsightsReport,
  ModelInsight,
  ModelPick,
  SessionsInsight,
  UseCasePick,
} from "../lib/types";
import { InsightsAnalysis } from "./InsightsAnalysis";
import { HealthPill, InsightsModelsTable } from "./InsightsModelsTable";
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
const TOP_COUNT = 3;
const QUALITY_WEIGHT = 0.5;
const STABILITY_WEIGHT = 0.3;
const SPEED_WEIGHT = 0.2;

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

function formatCostPerCall(cents: number | null): string {
  if (cents === null) {
    return "—";
  }
  return formatUsd(cents / 100);
}

function queryString(range: InsightsRange, minCalls: number): string {
  return `range=${range}&min_requests=${minCalls}`;
}

/** Resolves model ids from a top list to their rows, keeping the list order. */
function byIds(models: ReadonlyMap<string, ModelInsight>, ids: readonly string[]): ModelInsight[] {
  return ids.flatMap((id) => {
    const model = models.get(id);
    return model ? [model] : [];
  });
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
  const modelsById = new Map(models.map((model) => [model.model, model] as const));
  const ranked = models.filter((model) => model.ranked);
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
        Which model has been best, most reliable and best value, from real calls through the hub. Scores combine the
        model's capability tier, verified test and review outcomes, reliability and speed. No AI computes them.
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
                <span className="mono insights-pick-model">
                  <ModelName id={pick.model} />
                </span>
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
            <button type="button" className="btn small" onClick={() => onCopy(agentReportUrl)}>
              Copy agent report URL
            </button>
          </div>
        </Panel>
      </div>

      {report ? <InsightsAnalysis query={queryString(range, minCalls)} /> : null}

      {report ? <Leaders leaders={report.leaders} modelsById={modelsById} onCopy={onCopy} /> : null}

      {report ? <ValuePicks report={report} modelsById={modelsById} onCopy={onCopy} /> : null}

      {report && report.sessions.total > 0 ? (
        <Sessions sessions={report.sessions} modelsById={modelsById} onCopy={onCopy} />
      ) : null}

      {report && report.best_for.length > 0 ? <BestFor picks={report.best_for} modelsById={modelsById} onCopy={onCopy} /> : null}

      {ranked.length > 0 ? (
        <Panel
          title="Ranking"
          sub={`top ${TOP_COUNT} of ${formatNumber(ranked.length)} ranked · overall = 50% quality + 30% reliability + 20% speed`}
          actions={
            <div className="insights-legend dim" aria-hidden="true">
              <span className="insights-swatch quality" /> Quality
              <span className="insights-swatch reliability" /> Reliability
              <span className="insights-swatch speed" /> Speed
            </div>
          }
        >
          <p className="dim insights-panel-note">
            Quality is the public capability tier of the model family. Reliability is the 95% lower bound of the
            success rate, so a model with few calls scores lower. Speed compares first-token time and output speed with the best ranked model. The full list is under All
            models.
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
                {ranked.slice(0, TOP_COUNT).map((model, index) => (
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
          sub={`${formatNumber(models.length)} models, ranked first · click a column to sort`}
          actions={
            <span className="insights-abbr">
              TTFT = first token · Out = output speed · Prefill = prompt reading · Value = score per US cent per call
            </span>
          }
        >
          <InsightsModelsTable models={models} onCopy={onCopy} />
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

function ValuePicks({
  report,
  modelsById,
  onCopy,
}: {
  readonly report: InsightsReport;
  readonly modelsById: ReadonlyMap<string, ModelInsight>;
  readonly onCopy: (text: string) => void;
}) {
  const freeItems: TopListItem[] = byIds(modelsById, report.best_free).map((model) => ({
    model: model.model,
    value: formatNumber(model.overall_score ?? 0, 1),
    sub: `${formatPct(model.success_rate_pct)} success · ${formatTps(model.decode_tokens_per_sec_p50 ?? 0)}`,
  }));
  const valueItems: TopListItem[] = byIds(modelsById, report.best_value).map((model) => ({
    model: model.model,
    value: formatCompact(model.value_score ?? 0),
    sub: `${formatCostPerCall(model.cost_per_call_cents)} per call · overall ${formatNumber(model.overall_score ?? 0, 1)}`,
  }));
  return (
    <div className="insights-value-grid">
      <Panel title="Best free models" sub="run on a local profile, cost $0" className="insights-free">
        <TopList
          items={freeItems}
          onCopy={onCopy}
          empty="No free model is ranked yet. Local models appear here after enough calls."
          limit={TOP_COUNT}
        />
      </Panel>
      <Panel title="Best value" sub="overall score per US cent per call" className="insights-value">
        <TopList
          items={valueItems}
          onCopy={onCopy}
          empty="No paid model is ranked yet, or none has a priced call in this window."
          limit={TOP_COUNT}
        />
      </Panel>
    </div>
  );
}

function Sessions({
  sessions,
  modelsById,
  onCopy,
}: {
  readonly sessions: SessionsInsight;
  readonly modelsById: ReadonlyMap<string, ModelInsight>;
  readonly onCopy: (text: string) => void;
}) {
  const items: TopListItem[] = byIds(modelsById, sessions.best_models).map((model) => ({
    model: model.model,
    value: `${formatNumber(model.sessions_solved)}/${formatNumber(model.sessions_verified)}`,
    sub: "sessions solved",
  }));
  return (
    <Panel
      title="Sessions"
      sub="one session per agent worker · solved when its last test, review or user verdict passed"
      className="insights-sessions"
    >
      <div className="insights-session-grid">
        <div className="insights-session-kpis">
          <div>
            <span className="kpi-label">Sessions</span>
            <span className="kpi-value">{formatCompact(sessions.total)}</span>
          </div>
          <div>
            <span className="kpi-label">Verified</span>
            <span className="kpi-value">{formatCompact(sessions.verified)}</span>
          </div>
          <div>
            <span className="kpi-label">Solved</span>
            <span className="kpi-value">
              {sessions.solved_pct === null ? "—" : formatPct(sessions.solved_pct)}
            </span>
          </div>
        </div>
        <div>
          <span className="kpi-label">Best session models</span>
          <TopList
            items={items}
            onCopy={onCopy}
            empty="No model led three verified sessions yet. Smart Auto records verdicts."
            limit={TOP_COUNT}
          />
        </div>
      </div>
    </Panel>
  );
}

function BestFor({
  picks,
  modelsById,
  onCopy,
}: {
  readonly picks: readonly UseCasePick[];
  readonly modelsById: ReadonlyMap<string, ModelInsight>;
  readonly onCopy: (text: string) => void;
}) {
  return (
    <Panel title="Best model for each job" sub="top three per use case, from the model registry tags">
      <div className="insights-strip insights-cards">
        {picks.map((pick) => {
          const items: TopListItem[] = byIds(modelsById, pick.top).map((model) => ({
            model: model.model,
            value: pick.ranked ? formatNumber(model.overall_score ?? 0, 1) : formatNumber(model.requests),
            sub: pick.ranked ? `${formatPct(model.success_rate_pct)} success` : `${formatNumber(model.requests)} calls`,
          }));
          return (
            <div key={pick.use_case} className="insights-card" title={pick.reason}>
              <span className="kpi-label">Best for {pick.use_case}</span>
              <TopList
                items={items}
                onCopy={onCopy}
                empty="No model is tagged for this job yet."
                limit={TOP_COUNT}
              />
              <span className="kpi-sub">{pick.ranked ? "Ranked from calls" : "Declared only, not enough calls yet"}</span>
            </div>
          );
        })}
      </div>
    </Panel>
  );
}

function Harnesses({ harnesses }: { readonly harnesses: readonly HarnessInsight[] }) {
  const totalCalls = harnesses.reduce((sum, harness) => sum + harness.requests, 0);
  const shown = harnesses.slice(0, TOP_COUNT);
  return (
    <Panel
      title="By harness"
      sub={`top ${shown.length} of ${formatNumber(harnesses.length)} · which client sent the calls, told apart by its User-Agent and headers`}
    >
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
            {shown.map((harness) => (
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

interface LeaderSpec {
  readonly label: string;
  readonly ids: readonly string[];
  readonly value: (model: ModelInsight) => string;
  readonly sub: (model: ModelInsight) => string;
}

function Leaders({
  leaders,
  modelsById,
  onCopy,
}: {
  readonly leaders: InsightLeaders;
  readonly modelsById: ReadonlyMap<string, ModelInsight>;
  readonly onCopy: (text: string) => void;
}) {
  const specs: readonly LeaderSpec[] = [
    {
      label: "Best overall",
      ids: leaders.best_overall,
      value: (model) => formatNumber(model.overall_score ?? 0, 1),
      sub: (model) => `${formatPct(model.success_rate_pct)} success`,
    },
    {
      label: "Most reliable",
      ids: leaders.most_stable,
      value: (model) => formatPct(model.success_rate_pct),
      sub: (model) => `${formatNumber(model.requests)} calls`,
    },
    {
      label: "Best verified",
      ids: leaders.best_verified,
      value: (model) => formatPct(model.verified_pass_pct ?? 0),
      sub: (model) => `${formatCompact(model.verified_outcomes)} tests, reviews, user verdicts`,
    },
    {
      label: "Quickest first token",
      ids: leaders.fastest_first_token,
      value: (model) => formatMs(model.ttft_p50_ms ?? 0),
      sub: (model) => `p95 ${formatMs(model.ttft_p95_ms ?? 0)}`,
    },
    {
      label: "Fastest output",
      ids: leaders.fastest_decode,
      value: (model) => formatTps(model.decode_tokens_per_sec_p50 ?? 0),
      sub: (model) => `${formatNumber(model.requests)} calls`,
    },
    {
      label: "Fastest prompt reading",
      ids: leaders.fastest_prefill,
      value: (model) => formatTps(model.prefill_tokens_per_sec_p50 ?? 0),
      sub: (model) => `${formatPct(model.cache_hit_rate_pct)} cached`,
    },
    {
      label: "Most used",
      ids: leaders.most_used,
      value: (model) => `${formatNumber(model.requests)} calls`,
      sub: (model) => `${formatPct(model.success_rate_pct)} success`,
    },
  ];
  return (
    <div className="insights-leaders">
      {specs.map((spec) => {
        const items: TopListItem[] = byIds(modelsById, spec.ids).map((model) => ({
          model: model.model,
          value: spec.value(model),
          sub: spec.sub(model),
        }));
        return (
          <Panel key={spec.label} title={spec.label} className="insights-leader">
            <TopList items={items} onCopy={onCopy} empty="No ranked model yet." limit={TOP_COUNT} />
          </Panel>
        );
      })}
    </div>
  );
}

function RankingRow({ model, position }: { readonly model: ModelInsight; readonly position: number }) {
  const quality = QUALITY_WEIGHT * (model.quality_score ?? 0);
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
          aria-label={`Overall ${formatNumber(model.overall_score ?? 0, 1)}: quality ${formatNumber(quality, 1)}, reliability ${formatNumber(reliability, 1)}, speed ${formatNumber(speed, 1)}`}
        >
          <span className="insights-bar-part quality" style={{ width: `${quality}%` }} />
          <span className="insights-bar-part reliability" style={{ width: `${reliability}%` }} />
          <span className="insights-bar-part speed" style={{ width: `${speed}%` }} />
        </span>
      </td>
      <td className="num insights-score">{formatNumber(model.overall_score ?? 0, 1)}</td>
    </tr>
  );
}

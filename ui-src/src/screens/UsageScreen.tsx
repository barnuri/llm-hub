import { useEffect, useState } from "react";

import { KpiCard, type KpiTone } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import {
  UsageChartLegend,
  UsageTrendChart,
  defaultHidden,
  rankModels,
  shortModel,
} from "../components/UsageModelChart";
import { api } from "../lib/api";
import { formatDate, formatDateTime, formatNumber, formatPct, formatUsd } from "../lib/format";
import { navigate } from "../lib/router";
import type {
  CopilotQuota,
  CopilotUsageReport,
  SeriesPoint,
  StatsEntry,
  StatsSnapshot,
  UsageReport,
} from "../lib/types";
import "../styles/screens/overview.css";

const PAGE_SIZE = 25;
const SPARK_DAYS = 7;
const TOP_MODELS = 8;
const PREMIUM_QUOTA_ID = "premium_interactions";
const QUOTA_WARN_PCT = 20;
const QUOTA_DANGER_PCT = 5;
const ERROR_STATUS = 400;

type OverviewPeriod = "1d" | "7d";

interface DailyTotals {
  readonly requests: number[];
  readonly tokensIn: number[];
  readonly tokensOut: number[];
  readonly cost: number[];
}

function periodUsable(snapshot: StatsSnapshot | null, range: string): snapshot is StatsSnapshot {
  return snapshot !== null && snapshot.filterable && snapshot.range === range;
}

function dailyTotals(series: readonly SeriesPoint[]): DailyTotals {
  const byDay = new Map<number, { requests: number; tokensIn: number; tokensOut: number; cost: number }>();
  for (const point of series) {
    const day = byDay.get(point.ts_ms) ?? { requests: 0, tokensIn: 0, tokensOut: 0, cost: 0 };
    day.requests += point.requests;
    day.tokensIn += point.tokens_in;
    day.tokensOut += point.tokens_out;
    day.cost += point.cost_usd;
    byDay.set(point.ts_ms, day);
  }
  const days = [...byDay.entries()]
    .sort(([left], [right]) => left - right)
    .slice(-SPARK_DAYS)
    .map(([, totals]) => totals);
  return {
    requests: days.map((day) => day.requests),
    tokensIn: days.map((day) => day.tokensIn),
    tokensOut: days.map((day) => day.tokensOut),
    cost: days.map((day) => day.cost),
  };
}

function sum(values: readonly number[]): number {
  return values.reduce((total, value) => total + value, 0);
}

function pickQuota(report: CopilotUsageReport | null): CopilotQuota | null {
  const quotas = report?.quotas ?? [];
  return quotas.find((quota) => quota.id === PREMIUM_QUOTA_ID) ?? quotas[0] ?? null;
}

function quotaTone(quota: CopilotQuota): KpiTone {
  if (quota.remainingPercentage <= QUOTA_DANGER_PCT) {
    return "danger";
  }
  if (quota.remainingPercentage <= QUOTA_WARN_PCT) {
    return "warn";
  }
  return "default";
}

function quotaSub(quota: CopilotQuota): string {
  const left = `${formatPct(quota.remainingPercentage)} left`;
  const resetMs = quota.resetDate ? Date.parse(quota.resetDate) : Number.NaN;
  return Number.isFinite(resetMs) ? `${left} · resets ${formatDate(resetMs)}` : left;
}

interface TopModelsProps {
  readonly models: readonly StatsEntry[];
  readonly totalRequests: number;
}

function TopModels({ models, totalRequests }: TopModelsProps) {
  const top = [...models]
    .filter((entry) => entry.requests > 0)
    .sort((left, right) => right.requests - left.requests)
    .slice(0, TOP_MODELS);

  return (
    <Panel title="Top models" sub="By requests" className="top-models">
      {top.length === 0 ? (
        <p className="note">No model traffic in this window.</p>
      ) : (
        <div className="table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th>Model</th>
                <th className="num">Requests</th>
                <th>Share</th>
                <th className="num">Cost</th>
              </tr>
            </thead>
            <tbody>
              {top.map((entry) => {
                const share = totalRequests > 0 ? (entry.requests / totalRequests) * 100 : 0;
                return (
                  <tr key={entry.key}>
                    <td className="mono" title={entry.key}>
                      {shortModel(entry.key)}
                    </td>
                    <td className="num">{formatNumber(entry.requests)}</td>
                    <td>
                      <span className="share">
                        <span className="share-bar" aria-hidden="true">
                          <span className="share-fill" style={{ width: `${share}%` }} />
                        </span>
                        <span className="share-value">{formatPct(share)}</span>
                      </span>
                    </td>
                    <td className="num">{formatUsd(entry.cost_usd)}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

interface UsageScreenProps {
  readonly period: OverviewPeriod;
}

export function UsageScreen({ period }: UsageScreenProps) {
  const [today, setToday] = useState<StatsSnapshot | null>(null);
  const [week, setWeek] = useState<StatsSnapshot | null>(null);
  const [report, setReport] = useState<UsageReport | null>(null);
  const [copilot, setCopilot] = useState<CopilotUsageReport | null>(null);
  const [historyOff, setHistoryOff] = useState(false);
  const [loading, setLoading] = useState(true);
  const [page, setPage] = useState(0);
  const [hidden, setHidden] = useState<Set<string>>(() => new Set());
  const [legendReady, setLegendReady] = useState(false);
  const [showRequests, setShowRequests] = useState(true);
  const [showCost, setShowCost] = useState(true);

  useEffect(() => {
    let cancelled = false;

    const load = async () => {
      const [todayResult, weekResult, usageResult, copilotResult] = await Promise.allSettled([
        api<StatsSnapshot>("/api/stats?range=1d"),
        api<StatsSnapshot>("/api/stats?range=7d"),
        api<UsageReport>("/api/usage"),
        api<CopilotUsageReport>("/api/copilot/usage"),
      ]);
      if (cancelled) {
        return;
      }
      if (todayResult.status === "fulfilled") {
        setToday(todayResult.value);
      }
      if (weekResult.status === "fulfilled") {
        setWeek(weekResult.value);
      }
      if (usageResult.status === "fulfilled") {
        setReport(usageResult.value);
      } else {
        setHistoryOff(true);
      }
      if (copilotResult.status === "fulfilled") {
        setCopilot(copilotResult.value);
      }
      setLoading(false);
    };

    void load();
    return () => {
      cancelled = true;
    };
  }, []);

  const weekReady = periodUsable(week, "7d");
  const todayReady = periodUsable(today, "1d");
  const ranked = rankModels(weekReady ? week.models : (today?.models ?? []));

  useEffect(() => {
    if (legendReady || ranked.length === 0) {
      return;
    }
    if (!todayReady && !weekReady) {
      return;
    }
    setHidden(defaultHidden(ranked));
    setLegendReady(true);
  }, [legendReady, ranked, todayReady, weekReady]);

  const rows = report?.recent ?? [];
  const pageCount = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const pageRows = rows.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);

  const toggleModel = (key: string) => {
    setHidden((current) => {
      const next = new Set(current);
      if (next.has(key)) {
        next.delete(key);
        return next;
      }
      const visible = ranked.filter((model) => !next.has(model)).length;
      if (visible <= 1) {
        return current;
      }
      next.add(key);
      return next;
    });
  };

  const isWeek = period === "7d";
  const selected = isWeek ? week : today;
  const snapshot = periodUsable(selected, period) ? selected : null;
  const daily = dailyTotals(weekReady ? (week.series ?? []) : []);
  const periodSuffix = isWeek ? "· 7 days" : "today";
  const weekSub = (values: readonly number[], format: (value: number) => string): string | undefined => {
    if (values.length === 0) {
      return undefined;
    }
    return isWeek ? `${format(sum(values) / values.length)} per day avg` : `${format(sum(values))} in 7 days`;
  };
  const quota = pickQuota(copilot);
  const requestsOn = showRequests || !showCost;
  const costOn = showCost || !showRequests;

  if (loading) {
    return (
      <section className="tab active overview" aria-busy="true" aria-live="polite">
        <div className="kpis">
          {Array.from({ length: 5 }, (_, index) => (
            <div key={index} className="skeleton" />
          ))}
        </div>
        <div className="skeleton overview-skeleton-chart" />
      </section>
    );
  }

  return (
    <section className="tab active overview">
      {snapshot ? (
        <div className="kpis">
          <KpiCard
            label={`Requests ${periodSuffix}`}
            value={formatNumber(snapshot.overview.requests)}
            sub={weekSub(daily.requests, formatNumber)}
            series={daily.requests}
          />
          <KpiCard
            label={`Errors ${periodSuffix}`}
            value={formatNumber(snapshot.overview.errors)}
            sub={
              snapshot.overview.requests > 0
                ? `${formatPct((snapshot.overview.errors / snapshot.overview.requests) * 100)} of requests`
                : undefined
            }
            tone={snapshot.overview.errors > 0 ? "danger" : "default"}
            onClick={() => navigate(`/errors?range=${period}`)}
          />
          <KpiCard
            label={`Tokens in ${periodSuffix}`}
            value={formatNumber(snapshot.overview.tokens_in)}
            sub={weekSub(daily.tokensIn, formatNumber)}
            series={daily.tokensIn}
            tone="info"
          />
          <KpiCard
            label={`Tokens out ${periodSuffix}`}
            value={formatNumber(snapshot.overview.tokens_out)}
            sub={weekSub(daily.tokensOut, formatNumber)}
            series={daily.tokensOut}
            tone="info"
          />
          <KpiCard
            label={`Cost ${periodSuffix}`}
            value={formatUsd(snapshot.overview.cost_usd)}
            sub={weekSub(daily.cost, formatUsd)}
            series={daily.cost}
          />
          {quota ? (
            <KpiCard
              label={quota.unit === "aic" ? "Copilot credits" : "Copilot premium requests"}
              value={`${formatNumber(quota.used)} / ${formatNumber(quota.entitlement)}`}
              sub={quotaSub(quota)}
              tone={quotaTone(quota)}
            />
          ) : null}
        </div>
      ) : (
        <p className="note" role="status">
          Day filters need persistence — totals for this window are not available.
        </p>
      )}

      {snapshot ? (
        <div className="grid-main-side">
          <Panel title="Model usage" sub={isWeek ? "Per day, last 7 days" : "Per hour, last 24 hours"}>
            <UsageChartLegend
              ranked={ranked}
              hidden={hidden}
              onToggle={toggleModel}
              showRequests={requestsOn}
              showCost={costOn}
              onToggleRequests={() => setShowRequests((current) => !current)}
              onToggleCost={() => setShowCost((current) => !current)}
            />
            <UsageTrendChart
              key={period}
              title={isWeek ? "Last 7 days" : "Today"}
              hint={isWeek ? "Per day" : "Per hour"}
              bucket={isWeek ? "day" : "hour"}
              series={snapshot.series ?? []}
              ranked={ranked}
              hidden={hidden}
              showRequests={requestsOn}
              showCost={costOn}
              onToggle={toggleModel}
            />
          </Panel>
          <TopModels models={snapshot.models} totalRequests={snapshot.overview.requests} />
        </div>
      ) : null}

      {historyOff ? (
        <p className="note" role="status">
          Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to record usage history.
        </p>
      ) : (
        <Panel
          title="Recent requests"
          sub={formatNumber(rows.length)}
          actions={
            <div className="usage-pager">
              <button
                type="button"
                className="btn small usage-page-btn"
                disabled={safePage <= 0}
                onClick={() => setPage(safePage - 1)}
              >
                Prev
              </button>
              <span className="dim">
                Page {safePage + 1} of {pageCount}
              </span>
              <button
                type="button"
                className="btn small usage-page-btn"
                disabled={safePage >= pageCount - 1}
                onClick={() => setPage(safePage + 1)}
              >
                Next
              </button>
            </div>
          }
        >
          <div className="table-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Model</th>
                  <th className="num">Status</th>
                  <th className="num">Latency ms</th>
                  <th className="num">In</th>
                  <th className="num">Out</th>
                  <th className="num">Cost</th>
                </tr>
              </thead>
              <tbody>
                {pageRows.length === 0 ? (
                  <tr>
                    <td colSpan={7} className="dim">
                      No requests recorded yet.
                    </td>
                  </tr>
                ) : (
                  pageRows.map((row) => (
                    <tr key={`${row.ts_ms}-${row.model}-${row.status}-${row.latency_ms}`}>
                      <td className="mono nowrap">{formatDateTime(row.ts_ms)}</td>
                      <td className="mono">{row.model}</td>
                      <td className={row.status >= ERROR_STATUS ? "num t-err" : "num"}>{row.status}</td>
                      <td className="num">{formatNumber(row.latency_ms)}</td>
                      <td className="num">{formatNumber(row.tokens_in)}</td>
                      <td className="num">{formatNumber(row.tokens_out)}</td>
                      <td className="num">{formatUsd(row.cost_usd)}</td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        </Panel>
      )}
    </section>
  );
}

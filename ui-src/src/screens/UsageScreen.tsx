import { useEffect, useMemo, useState } from "react";

import { KpiCard, type KpiTone } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { UpstreamBarsChart } from "../components/UpstreamBarsChart";
import {
  UsageChartLegend,
  UsageTrendChart,
  defaultHidden,
  rankModels,
  shortModel,
} from "../components/UsageModelChart";
import { api } from "../lib/api";
import { formatDate, formatDateTime, formatMs, formatNumber, formatPct, formatUsd } from "../lib/format";
import { navigate, setQueryParam } from "../lib/router";
import { formatBucket, type SeriesBucket, stackByUpstream, type UpstreamStacks } from "../lib/series";
import type {
  CopilotQuota,
  CopilotUsageReport,
  ErrorsReport,
  SeriesPoint,
  StatsEntry,
  StatsSnapshot,
  UsageReport,
  UsageRow,
} from "../lib/types";
import "../styles/screens/overview.css";

const PAGE_SIZE = 25;
const SPARK_DAYS = 7;
const DAY_MS = 86_400_000;
const TOP_MODELS = 8;
const PREMIUM_QUOTA_ID = "premium_interactions";
const QUOTA_WARN_PCT = 20;
const QUOTA_DANGER_PCT = 5;
const ERROR_STATUS = 400;
const KPI_COUNT = 6;

export type OverviewPeriod = "1d" | "7d";

type ChartView = "upstreams" | "models";

const PERIOD_OPTIONS: readonly SegmentedOption<OverviewPeriod>[] = [
  { value: "1d", label: "24 h" },
  { value: "7d", label: "7 d" },
];

const VIEW_OPTIONS: readonly SegmentedOption<ChartView>[] = [
  { value: "upstreams", label: "Upstreams" },
  { value: "models", label: "Models" },
];

interface DailyTotals {
  readonly days: number[];
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
  const days = [...byDay.entries()].sort(([left], [right]) => left - right).slice(-SPARK_DAYS);
  return {
    days: days.map(([ts]) => ts),
    requests: days.map(([, totals]) => totals.requests),
    tokensIn: days.map(([, totals]) => totals.tokensIn),
    tokensOut: days.map(([, totals]) => totals.tokensOut),
    cost: days.map(([, totals]) => totals.cost),
  };
}

/** Errors per series day; the errors endpoint returns at most its recent-row limit, so busy weeks undercount. */
function dailyErrors(days: readonly number[], rows: readonly UsageRow[]): number[] {
  return days.map((day) => rows.filter((row) => Math.floor(row.ts_ms / DAY_MS) * DAY_MS === day).length);
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

function quotaUnit(quota: CopilotQuota): string {
  return quota.unit === "aic" ? "AIC" : "requests";
}

function quotaUsedPct(quota: CopilotQuota): number {
  return Math.min(100, Math.max(0, 100 - quota.remainingPercentage));
}

function quotaReset(quota: CopilotQuota): string | null {
  const resetMs = quota.resetDate ? Date.parse(quota.resetDate) : Number.NaN;
  return Number.isFinite(resetMs) ? formatDate(resetMs) : null;
}

function formatClock(date: Date): string {
  return date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
}

function hoverReadout(stacks: UpstreamStacks, index: number | null, bucket: SeriesBucket): string | null {
  const entry = index === null ? undefined : stacks.buckets[index];
  if (!entry) {
    return null;
  }
  const parts = stacks.groups.map(
    (group, groupIndex) => `${group.label} ${formatNumber(entry.values[groupIndex] ?? 0)}`,
  );
  return [formatBucket(entry.ts, bucket), `${formatNumber(entry.total)} req`, ...parts].join(" · ");
}

interface QuotaPanelProps {
  readonly quota: CopilotQuota | null;
}

function QuotaPanel({ quota }: QuotaPanelProps) {
  if (!quota) {
    return (
      <Panel title="Copilot quota">
        <p className="note">No Copilot quota available. Add a GitHub Copilot profile to track it.</p>
      </Panel>
    );
  }
  const used = quotaUsedPct(quota);
  const tone = quotaTone(quota);
  const reset = quotaReset(quota);
  return (
    <Panel title="Copilot quota" sub={quota.unit === "aic" ? "AI credits" : "Premium requests"} className="quota">
      <div className="quota-value">
        <span className="quota-pct">{formatPct(used)}</span>
        <span className="dim">used</span>
      </div>
      <div
        className={tone === "default" ? "quota-bar" : `quota-bar tone-${tone}`}
        role="meter"
        aria-label="Copilot quota used"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(used)}
      >
        <span className="quota-fill" style={{ width: `${used}%` }} />
      </div>
      <div className="quota-meta">
        <span className="mono">
          {formatNumber(quota.used)} / {formatNumber(quota.entitlement)} {quotaUnit(quota)}
        </span>
        {reset ? <span className="dim">Resets {reset}</span> : null}
      </div>
    </Panel>
  );
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
              </tr>
            </thead>
            <tbody>
              {top.map((entry) => {
                const share = totalRequests > 0 ? (entry.requests / totalRequests) * 100 : 0;
                return (
                  <tr key={entry.key}>
                    <td className="mono" title={`${entry.key} · ${formatUsd(entry.cost_usd)}`}>
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
  const [weekErrors, setWeekErrors] = useState<readonly UsageRow[] | null>(null);
  const [loadedAt, setLoadedAt] = useState<Date | null>(null);
  const [historyOff, setHistoryOff] = useState(false);
  const [loading, setLoading] = useState(true);
  const [page, setPage] = useState(0);
  const [hidden, setHidden] = useState<Set<string>>(() => new Set());
  const [legendReady, setLegendReady] = useState(false);
  const [showRequests, setShowRequests] = useState(true);
  const [showCost, setShowCost] = useState(true);
  const [view, setView] = useState<ChartView>("upstreams");
  const [hoveredBar, setHoveredBar] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;

    const load = async () => {
      const [todayResult, weekResult, usageResult, copilotResult, errorsResult] = await Promise.allSettled([
        api<StatsSnapshot>("/api/stats?range=1d"),
        api<StatsSnapshot>("/api/stats?range=7d"),
        api<UsageReport>("/api/usage"),
        api<CopilotUsageReport>("/api/copilot/usage"),
        api<ErrorsReport>("/api/errors?range=7d"),
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
      if (errorsResult.status === "fulfilled") {
        setWeekErrors(errorsResult.value.recent);
      }
      setLoadedAt(new Date());
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

  const isWeek = period === "7d";
  const bucket: SeriesBucket = isWeek ? "day" : "hour";
  const selected = isWeek ? week : today;
  const snapshot = periodUsable(selected, period) ? selected : null;
  const series = snapshot?.series;
  const stacks = useMemo(() => stackByUpstream(series ?? []), [series]);

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

  const changePeriod = (value: OverviewPeriod) => {
    setHoveredBar(null);
    setQueryParam("range", value);
  };

  const daily = dailyTotals(weekReady ? (week.series ?? []) : []);
  const errorSeries = weekErrors ? dailyErrors(daily.days, weekErrors) : [];
  const weekSub = (values: readonly number[], format: (value: number) => string): string | undefined => {
    if (values.length === 0) {
      return undefined;
    }
    return isWeek ? `${format(sum(values) / values.length)} per day avg` : `${format(sum(values))} in 7 days`;
  };
  const quota = pickQuota(copilot);
  const requestsOn = showRequests || !showCost;
  const costOn = showCost || !showRequests;
  const windowLabel = isWeek ? "last 7 days" : "last 24 hours";
  const readout = view === "upstreams" ? hoverReadout(stacks, hoveredBar, bucket) : null;

  const pageHead = (
    <div className="page-head">
      <h1 className="page-title">Overview</h1>
      <span className="page-sub mono">
        {isWeek ? "last 7 days" : "today"}
        {loadedAt ? ` · updated ${formatClock(loadedAt)}` : ""}
      </span>
    </div>
  );

  if (loading) {
    return (
      <section className="tab active overview" aria-busy="true" aria-live="polite">
        {pageHead}
        <div className="kpi-strip">
          {Array.from({ length: KPI_COUNT }, (_, index) => (
            <div key={index} className="skeleton kpi-skeleton" />
          ))}
        </div>
        <div className="skeleton overview-skeleton-chart" />
      </section>
    );
  }

  return (
    <section className="tab active overview">
      {pageHead}
      {snapshot ? (
        <div className="kpi-strip">
          <KpiCard
            label="Requests"
            value={formatNumber(snapshot.overview.requests)}
            sub={weekSub(daily.requests, formatNumber)}
            series={daily.requests}
          />
          <KpiCard
            label="Errors"
            value={formatNumber(snapshot.overview.errors)}
            sub={
              snapshot.overview.requests > 0
                ? `${formatPct((snapshot.overview.errors / snapshot.overview.requests) * 100)} of requests`
                : "No requests yet"
            }
            series={errorSeries}
            tone="danger"
            onClick={() => navigate(`/errors?range=${period}`)}
          />
          <KpiCard
            label="Tokens in"
            value={formatNumber(snapshot.overview.tokens_in)}
            sub={weekSub(daily.tokensIn, formatNumber)}
            series={daily.tokensIn}
          />
          <KpiCard
            label="Tokens out"
            value={formatNumber(snapshot.overview.tokens_out)}
            sub={weekSub(daily.tokensOut, formatNumber)}
            series={daily.tokensOut}
          />
          <KpiCard
            label="Cost"
            value={formatUsd(snapshot.overview.cost_usd)}
            sub={weekSub(daily.cost, formatUsd)}
            series={daily.cost}
          />
          {quota ? (
            <KpiCard
              label="Copilot quota"
              value={formatPct(quotaUsedPct(quota))}
              sub={`${formatNumber(quota.used)} / ${formatNumber(quota.entitlement)} ${quotaUnit(quota)}`}
              tone={quotaTone(quota)}
            />
          ) : (
            <KpiCard label="Copilot quota" value="—" sub="Not connected" />
          )}
        </div>
      ) : (
        <p className="note" role="status">
          Day filters need persistence — totals for this window are not available.
        </p>
      )}

      {snapshot ? (
        <div className="overview-grid">
          <Panel
            title={view === "upstreams" ? `Requests by upstream · ${windowLabel}` : `Model usage · ${windowLabel}`}
            sub={readout ?? undefined}
            className="overview-chart"
            actions={
              <>
                <Segmented label="Chart" options={VIEW_OPTIONS} value={view} onChange={setView} />
                <Segmented label="Period" options={PERIOD_OPTIONS} value={period} onChange={changePeriod} />
              </>
            }
          >
            {view === "upstreams" ? (
              <UpstreamBarsChart stacks={stacks} bucket={bucket} hovered={hoveredBar} onHover={setHoveredBar} />
            ) : (
              <>
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
                  bucket={bucket}
                  series={series ?? []}
                  ranked={ranked}
                  hidden={hidden}
                  showRequests={requestsOn}
                  showCost={costOn}
                  onToggle={toggleModel}
                />
              </>
            )}
          </Panel>
          <div className="overview-side">
            <QuotaPanel quota={quota} />
            <TopModels models={snapshot.models} totalRequests={snapshot.overview.requests} />
          </div>
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
          className="recent-requests"
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
                  <th>Client</th>
                  <th>Status</th>
                  <th className="num">TTFT</th>
                  <th className="num">Latency</th>
                  <th className="num">Tokens in / out</th>
                  <th className="num">Cost</th>
                </tr>
              </thead>
              <tbody>
                {pageRows.length === 0 ? (
                  <tr>
                    <td colSpan={8} className="dim">
                      No requests recorded yet.
                    </td>
                  </tr>
                ) : (
                  pageRows.map((row) => (
                    <tr key={`${row.ts_ms}-${row.model}-${row.status}-${row.latency_ms}`}>
                      <td className="mono nowrap">{formatDateTime(row.ts_ms)}</td>
                      <td className="mono">{row.model}</td>
                      <td className="client" title={row.client ?? undefined}>
                        {row.client ?? <span className="dim">—</span>}
                      </td>
                      <td>
                        <Pill tone={row.status >= ERROR_STATUS ? "danger" : "ok"} title={row.error ?? undefined}>
                          {row.status}
                        </Pill>
                      </td>
                      <td className="num">{formatMs(row.ttft_ms ?? 0)}</td>
                      <td className="num">{formatMs(row.latency_ms)}</td>
                      <td className="num nowrap">
                        {formatNumber(row.tokens_in)} / {formatNumber(row.tokens_out)}
                      </td>
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

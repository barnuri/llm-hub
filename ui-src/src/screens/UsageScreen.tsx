import { useEffect, useState } from "react";

import {
  UsageChartLegend,
  UsageTrendChart,
  defaultHidden,
  rankModels,
} from "../components/UsageModelChart";
import { api } from "../lib/api";
import { formatNumber, formatTime, formatUsd } from "../lib/format";
import { navigate } from "../lib/router";
import type { SeriesPoint, StatsSnapshot, UsageReport } from "../lib/types";

const PAGE_SIZE = 25;

function periodUsable(snapshot: StatsSnapshot | null, range: string): snapshot is StatsSnapshot {
  return snapshot !== null && snapshot.filterable && snapshot.range === range;
}

interface PeriodTilesProps {
  readonly title: string;
  readonly hint: string;
  readonly snapshot: StatsSnapshot | null;
  readonly range: string;
}

function PeriodTiles({ title, hint, snapshot, range }: PeriodTilesProps) {
  if (!periodUsable(snapshot, range)) {
    return (
      <section className="usage-period">
        <h2>{title}</h2>
        <p className="dim">{hint}</p>
        <p className="note" role="status">
          Day filters need persistence — totals for this window are not available.
        </p>
      </section>
    );
  }

  const overview = snapshot.overview;
  const tiles: ReadonlyArray<readonly [string, string, string, boolean]> = [
    ["Requests", formatNumber(overview.requests), "", false],
    ["Errors", formatNumber(overview.errors), overview.errors > 0 ? "accent-warn" : "", true],
    ["Tokens in", formatNumber(overview.tokens_in), "", false],
    ["Tokens out", formatNumber(overview.tokens_out), "", false],
    ["Cost", formatUsd(overview.cost_usd), "accent-accent", false],
  ];

  return (
    <section className="usage-period">
      <h2>{title}</h2>
      <p className="dim">{hint}</p>
      <div className="stat-tiles usage-kpis">
        {tiles.map(([label, value, accent, opensErrors]) => {
          const className = accent ? `stat-tile ${accent}` : "stat-tile";
          if (opensErrors) {
            return (
              <button
                key={label}
                type="button"
                className={`${className} stat-tile-link`}
                onClick={() => navigate(`/errors?range=${range}`)}
              >
                <div className="value">{value}</div>
                <div className="label">{label}</div>
              </button>
            );
          }
          return (
            <div key={label} className={className}>
              <div className="value">{value}</div>
              <div className="label">{label}</div>
            </div>
          );
        })}
      </div>
    </section>
  );
}

export function UsageScreen() {
  const [today, setToday] = useState<StatsSnapshot | null>(null);
  const [week, setWeek] = useState<StatsSnapshot | null>(null);
  const [report, setReport] = useState<UsageReport | null>(null);
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
      const [todayResult, weekResult, usageResult] = await Promise.allSettled([
        api<StatsSnapshot>("/api/stats?range=1d"),
        api<StatsSnapshot>("/api/stats?range=7d"),
        api<UsageReport>("/api/usage"),
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

  const todaySeries: readonly SeriesPoint[] = today?.series ?? [];
  const weekSeries: readonly SeriesPoint[] = week?.series ?? [];
  const requestsOn = showRequests || !showCost;
  const costOn = showCost || !showRequests;

  if (loading) {
    return (
      <section className="tab active" aria-busy="true" aria-live="polite">
        <div className="usage-skeleton" />
        <div className="usage-skeleton usage-skeleton-chart" />
      </section>
    );
  }

  return (
    <section className="tab active">
      <div className="usage-periods">
        <PeriodTiles title="Today" hint="Last 24 hours" snapshot={today} range="1d" />
        <PeriodTiles title="Last 7 days" hint="Rolling 7-day window" snapshot={week} range="7d" />
      </div>

      {todayReady || weekReady ? (
        <section className="usage-period">
          <h2>Model usage</h2>
          <p className="dim">Hourly last 24 hours, daily last 7 days — click a model to show or hide it</p>
          <UsageChartLegend
            ranked={ranked}
            hidden={hidden}
            onToggle={toggleModel}
            showRequests={requestsOn}
            showCost={costOn}
            onToggleRequests={() => setShowRequests((current) => !current)}
            onToggleCost={() => setShowCost((current) => !current)}
          />
          <div className="usage-charts">
            {todayReady ? (
              <UsageTrendChart
                title="Today"
                hint="Per hour"
                bucket="hour"
                series={todaySeries}
                ranked={ranked}
                hidden={hidden}
                showRequests={requestsOn}
                showCost={costOn}
                onToggle={toggleModel}
              />
            ) : null}
            {weekReady ? (
              <UsageTrendChart
                title="Last 7 days"
                hint="Per day"
                bucket="day"
                series={weekSeries}
                ranked={ranked}
                hidden={hidden}
                showRequests={requestsOn}
                showCost={costOn}
                onToggle={toggleModel}
              />
            ) : null}
          </div>
        </section>
      ) : null}

      {historyOff ? (
        <p className="note" role="status">
          Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to record usage history.
        </p>
      ) : (
        <details className="usage-records">
          <summary>Recent requests ({formatNumber(rows.length)})</summary>
          <div className="toolbar usage-pager">
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
          <div className="table-scroll usage-table-scroll">
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
                      <td className="mono">{formatTime(row.ts_ms)}</td>
                      <td className="mono">{row.model}</td>
                      <td className="num">{row.status}</td>
                      <td className="num">{row.latency_ms}</td>
                      <td className="num">{row.tokens_in}</td>
                      <td className="num">{row.tokens_out}</td>
                      <td className="num">{formatUsd(row.cost_usd)}</td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        </details>
      )}
    </section>
  );
}

import { useEffect, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { ShareBar } from "../components/ShareBar";
import { api } from "../lib/api";
import { formatDateTime, formatMs, formatNumber, formatPct } from "../lib/format";
import { useLive } from "../lib/live";
import { navigate, useRoute } from "../lib/router";
import type { ErrorsRange, ErrorsReport, StatsSnapshot, UsageRow } from "../lib/types";
import "../styles/screens/errors.css";

const PAGE_SIZE = 25;
const TOP_REASONS = 5;
const UNRECORDED_REASON = "Reason not recorded (before error capture)";
const DAY_MS = 86_400_000;
const SPARK_DAYS = 7;
const PERCENT = 100;
const RANGES: readonly SegmentedOption<ErrorsRange>[] = [
  { value: "1d", label: "Today" },
  { value: "7d", label: "7 days" },
  { value: "all", label: "All" },
];

interface ReasonGroup {
  readonly reason: string;
  readonly count: number;
  readonly models: readonly string[];
  readonly lastTsMs: number;
}

function reasonOf(row: UsageRow): string {
  return row.error ?? UNRECORDED_REASON;
}

function groupReasons(rows: readonly UsageRow[]): readonly ReasonGroup[] {
  const groups = new Map<string, { count: number; models: Set<string>; lastTsMs: number }>();
  for (const row of rows) {
    const reason = reasonOf(row);
    const group = groups.get(reason) ?? { count: 0, models: new Set<string>(), lastTsMs: 0 };
    group.count += 1;
    group.models.add(row.model);
    group.lastTsMs = Math.max(group.lastTsMs, row.ts_ms);
    groups.set(reason, group);
  }
  return [...groups.entries()]
    .map(([reason, group]) => ({ reason, count: group.count, models: [...group.models], lastTsMs: group.lastTsMs }))
    .sort((a, b) => b.count - a.count || b.lastTsMs - a.lastTsMs);
}

function dayOf(tsMs: number): number {
  return Math.floor(tsMs / DAY_MS) * DAY_MS;
}

function lastDays(nowMs: number): number[] {
  const today = dayOf(nowMs);
  return Array.from({ length: SPARK_DAYS }, (_, index) => today - (SPARK_DAYS - 1 - index) * DAY_MS);
}

function dailyErrors(days: readonly number[], rows: readonly UsageRow[]): number[] {
  return days.map((day) => rows.filter((row) => dayOf(row.ts_ms) === day).length);
}

function dailyModels(days: readonly number[], rows: readonly UsageRow[]): number[] {
  return days.map((day) => new Set(rows.filter((row) => dayOf(row.ts_ms) === day).map((row) => row.model)).size);
}

function parseRange(value: string | null): ErrorsRange {
  if (value === "1d" || value === "7d" || value === "30d" || value === "all") {
    return value;
  }
  return "7d";
}

export function ErrorsScreen() {
  const route = useRoute();
  const range = parseRange(route.query.get("range"));
  const [report, setReport] = useState<ErrorsReport | null>(null);
  const [loadedAtMs, setLoadedAtMs] = useState(0);
  const [unavailable, setUnavailable] = useState(false);
  const [page, setPage] = useState(0);
  const [reasonFilter, setReasonFilter] = useState<string | null>(null);
  const [showAllReasons, setShowAllReasons] = useState(false);
  const [calls, setCalls] = useState<StatsSnapshot | null>(null);
  const { tick } = useLive();

  useEffect(() => {
    let cancelled = false;
    setUnavailable(false);
    api<ErrorsReport>(`/api/errors?range=${range}`)
      .then((body) => {
        if (!cancelled) {
          setReport(body);
          setLoadedAtMs(Date.now());
        }
      })
      .catch(() => {
        if (!cancelled) {
          setUnavailable(true);
          setReport(null);
        }
      });
    api<StatsSnapshot>(`/api/stats?range=${range}`)
      .then((body) => {
        if (!cancelled) {
          setCalls(body);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          console.warn("Call totals unavailable for the error rate", err);
          setCalls(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [range, tick]);

  useEffect(() => {
    setPage(0);
  }, [range]);

  if (unavailable) {
    return (
      <section className="tab active errors">
        <Panel title="Errors unavailable">
          <p className="note" role="status">
            Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to record failed requests.
          </p>
        </Panel>
      </section>
    );
  }

  const allRows = report?.recent ?? [];
  const reasons = groupReasons(allRows);
  const rows = reasonFilter ? allRows.filter((row) => reasonOf(row) === reasonFilter) : allRows;
  const pageCount = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const pageRows = rows.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);

  const toggleReason = (reason: string) => {
    setReasonFilter((current) => (current === reason ? null : reason));
    setPage(0);
  };

  const totalErrors = report?.total_errors ?? 0;
  const modelsAffected = new Set(allRows.map((row) => row.model)).size;
  const lastError = allRows.reduce<UsageRow | null>(
    (latest, row) => (latest === null || row.ts_ms > latest.ts_ms ? row : latest),
    null,
  );
  const rangeLabel = RANGES.find((option) => option.value === range)?.label ?? range;
  const totalCalls = calls?.filterable && calls.range === range ? calls.overview.requests : 0;
  const errorsSub =
    totalCalls > 0
      ? `${rangeLabel} · ${formatPct((totalErrors / totalCalls) * PERCENT)} of ${formatNumber(totalCalls)} calls`
      : rangeLabel;
  const sparkDays = report && range === "7d" && allRows.length === totalErrors ? lastDays(loadedAtMs) : null;
  const shownReasons = showAllReasons ? reasons : reasons.slice(0, TOP_REASONS);
  const reasonsSub =
    reasons.length > TOP_REASONS && !showAllReasons
      ? `Select a reason to show only those calls · top ${TOP_REASONS} of ${formatNumber(reasons.length)}`
      : "Select a reason to show only those calls";

  return (
    <section className="tab active errors" aria-busy={report === null}>
      <div className="toolbar errors-toolbar">
        <Segmented
          label="Errors period"
          options={RANGES}
          value={range}
          onChange={(value) => navigate(`/errors?range=${value}`)}
        />
        <span className="dim">Failed calls (HTTP 400+) in the selected window</span>
      </div>

      <div className="errors-kpis">
        <KpiCard
          label="Errors"
          value={report ? formatNumber(totalErrors) : "…"}
          sub={errorsSub}
          tone={totalErrors > 0 ? "danger" : "default"}
          series={sparkDays ? dailyErrors(sparkDays, allRows) : undefined}
          seriesLabel="Errors per day, last 7 days"
        />
        <KpiCard label="Distinct reasons" value={report ? formatNumber(reasons.length) : "…"} sub="In recent failures" />
        <KpiCard
          label="Models affected"
          value={report ? formatNumber(modelsAffected) : "…"}
          sub="In recent failures"
          series={sparkDays ? dailyModels(sparkDays, allRows) : undefined}
          seriesLabel="Models with a failure per day, last 7 days"
        />
        <KpiCard
          label="Last error"
          value={lastError ? formatDateTime(lastError.ts_ms) : "None"}
          sub={lastError ? `Most recent failed call · ${lastError.model}` : "No failures in this window"}
        />
      </div>

      <div className="stack errors-body">
        {reasons.length > 0 ? (
          <Panel
            title="Top reasons"
            sub={reasonsSub}
            actions={
              <>
                {reasonFilter ? (
                  <button
                    type="button"
                    className="chip errors-filter-chip"
                    title="Clear the reason filter"
                    onClick={() => toggleReason(reasonFilter)}
                  >
                    Filter: <span className="errors-filter-text">{reasonFilter}</span>
                    <span aria-hidden="true">×</span>
                  </button>
                ) : null}
                {reasons.length > TOP_REASONS ? (
                  <button type="button" className="btn small" onClick={() => setShowAllReasons((shown) => !shown)}>
                    {showAllReasons ? `Show top ${TOP_REASONS}` : "Show all reasons"}
                  </button>
                ) : null}
              </>
            }
          >
            <div className="table-scroll">
              <table className="table errors-reasons">
                <thead>
                  <tr>
                    <th className="num">Count</th>
                    <th>Share</th>
                    <th>Reason</th>
                    <th>Models</th>
                    <th>Last</th>
                    <th>
                      <span className="sr-only">Filter</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {shownReasons.map((group) => {
                    const active = reasonFilter === group.reason;
                    const share = allRows.length > 0 ? group.count / allRows.length : 0;
                    return (
                      <tr key={group.reason} className={active ? "errors-reason-active" : undefined}>
                        <td className="num errors-reason-count">{formatNumber(group.count)}×</td>
                        <td>
                          <ShareBar fraction={share} label={`${formatPct(share * PERCENT)} of failures`} />
                        </td>
                        <td className="mono errors-reason-text" title={group.reason}>
                          {group.reason}
                        </td>
                        <td className="mono errors-reason-models">{group.models.join(", ")}</td>
                        <td className="nowrap errors-reason-last">{formatDateTime(group.lastTsMs)}</td>
                        <td>
                          <button
                            type="button"
                            className={active ? "chip active" : "chip"}
                            aria-pressed={active}
                            aria-label={active ? `Stop filtering by ${group.reason}` : `Filter by ${group.reason}`}
                            onClick={() => toggleReason(group.reason)}
                          >
                            {active ? "On" : "Filter"}
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </Panel>
        ) : null}

        <Panel
          title="Failed calls"
          sub={`${formatNumber(rows.length)} shown${reasonFilter ? ", filtered by reason" : ""}`}
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
          <div className="table-scroll errors-table-scroll">
            <table className="table errors-table">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Model</th>
                  <th className="num">Status</th>
                  <th className="num">Latency</th>
                  <th>Reason</th>
                </tr>
              </thead>
              <tbody>
                {pageRows.length === 0 ? (
                  <tr>
                    <td colSpan={5} className="dim errors-empty">
                      {report === null ? "Loading…" : "No errors in this window."}
                    </td>
                  </tr>
                ) : (
                  pageRows.map((row) => (
                    <tr key={`${row.ts_ms}-${row.model}-${row.status}-${row.latency_ms}`}>
                      <td className="mono nowrap">{formatDateTime(row.ts_ms)}</td>
                      <td className="mono nowrap">{row.model}</td>
                      <td className="num t-err">{row.status}</td>
                      <td className="num nowrap">{formatMs(row.latency_ms)}</td>
                      <td className="mono errors-call-reason" title={row.error ?? undefined}>
                        {row.error ?? <span className="dim">not recorded</span>}
                      </td>
                    </tr>
                  ))
                )}
              </tbody>
            </table>
          </div>
        </Panel>
      </div>
    </section>
  );
}

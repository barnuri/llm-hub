import { useEffect, useState } from "react";

import { api } from "../lib/api";
import { formatDateTime, formatMs, formatNumber } from "../lib/format";
import { navigate, useRoute } from "../lib/router";
import type { ErrorsRange, ErrorsReport, UsageRow } from "../lib/types";

const PAGE_SIZE = 25;
const TOP_REASONS = 5;
const UNRECORDED_REASON = "Reason not recorded (before error capture)";
const RANGES: ReadonlyArray<readonly [ErrorsRange, string]> = [
  ["1d", "Today"],
  ["7d", "Last 7 days"],
  ["all", "All"],
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
  const [unavailable, setUnavailable] = useState(false);
  const [page, setPage] = useState(0);
  const [reasonFilter, setReasonFilter] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setUnavailable(false);
    api<ErrorsReport>(`/api/errors?range=${range}`)
      .then((body) => {
        if (!cancelled) {
          setReport(body);
          setPage(0);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setUnavailable(true);
          setReport(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [range]);

  if (unavailable) {
    return (
      <section className="tab active">
        <h2>Errors</h2>
        <p className="note" role="status">
          Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to record failed requests.
        </p>
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

  return (
    <section className="tab active">
      <h2>Errors</h2>
      <p className="dim">Failed calls (HTTP 400+) in the selected window</p>
      <div className="toolbar">
        {RANGES.map(([id, label]) => (
          <button
            key={id}
            type="button"
            className={range === id ? "chip active" : "chip"}
            onClick={() => navigate(`/errors?range=${id}`)}
          >
            {label}
          </button>
        ))}
        <span className="dim">
          {formatNumber(report?.total_errors ?? 0)} error{(report?.total_errors ?? 0) === 1 ? "" : "s"}
        </span>
      </div>

      {reasons.length > 0 ? (
        <div className="errors-reasons">
          <h3>Top reasons</h3>
          <ul>
            {reasons.slice(0, TOP_REASONS).map((group) => (
              <li key={group.reason}>
                <button
                  type="button"
                  className={reasonFilter === group.reason ? "errors-reason active" : "errors-reason"}
                  aria-pressed={reasonFilter === group.reason}
                  onClick={() => toggleReason(group.reason)}
                >
                  <span className="errors-reason-count mono">{formatNumber(group.count)}×</span>
                  <span className="errors-reason-text">{group.reason}</span>
                  <span className="dim errors-reason-meta">
                    {group.models.join(", ")}. Last {formatDateTime(group.lastTsMs)}
                  </span>
                </button>
              </li>
            ))}
          </ul>
          {reasonFilter ? (
            <button type="button" className="btn small" onClick={() => toggleReason(reasonFilter)}>
              Show all reasons
            </button>
          ) : (
            <p className="dim">Select a reason to show only those calls.</p>
          )}
        </div>
      ) : null}

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
      <div className="table-scroll">
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
                <td colSpan={5} className="dim">
                  No errors in this window.
                </td>
              </tr>
            ) : (
              pageRows.map((row) => (
                <tr key={`${row.ts_ms}-${row.model}-${row.status}-${row.latency_ms}`}>
                  <td className="mono nowrap">{formatDateTime(row.ts_ms)}</td>
                  <td className="mono nowrap">{row.model}</td>
                  <td className="num">{row.status}</td>
                  <td className="num nowrap">{formatMs(row.latency_ms)}</td>
                  <td className="mono error-reason" title={row.error ?? undefined}>
                    {row.error ?? <span className="dim">not recorded</span>}
                  </td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}

import { useEffect, useState } from "react";

import { api } from "../lib/api";
import { formatNumber, formatTime, formatUsd } from "../lib/format";
import { navigate, useRoute } from "../lib/router";
import type { ErrorsRange, ErrorsReport } from "../lib/types";

const PAGE_SIZE = 25;
const RANGES: ReadonlyArray<readonly [ErrorsRange, string]> = [
  ["1d", "Today"],
  ["7d", "Last 7 days"],
  ["all", "All"],
];

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

  const rows = report?.recent ?? [];
  const pageCount = Math.max(1, Math.ceil(rows.length / PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const pageRows = rows.slice(safePage * PAGE_SIZE, (safePage + 1) * PAGE_SIZE);

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
                  No errors in this window.
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
    </section>
  );
}

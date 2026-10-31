import type { StatsEntry } from "../lib/types";
import { formatMs, formatNumber, formatTps, formatUsd } from "../lib/format";
import { Panel } from "./Panel";

interface StatsTableProps {
  readonly label: string;
  readonly rows: readonly StatsEntry[];
}

export function StatsTable({ label, rows }: StatsTableProps) {
  return (
    <Panel
      className="stats-table"
      title={`By ${label.toLowerCase()}`}
      sub={rows.length === 1 ? "1 row" : `${formatNumber(rows.length)} rows`}
    >
      <div className="table-scroll">
        <table className="table">
          <thead>
            <tr>
              <th>{label}</th>
              <th className="num" title="How many times this was called">
                Requests
              </th>
              <th className="num" title="Estimated USD for tokens in this row">
                Cost
              </th>
              <th className="num" title="Calls that failed (HTTP 400+)">
                Errors
              </th>
              <th className="num" title="Time to first token — how long until the reply starts streaming">
                TTFT p50
              </th>
              <th className="num" title="How fast tokens arrive after the first one">
                Tok/s p50
              </th>
              <th className="num" title="Prompt tokens served from cache (cheaper / faster)">
                Cache read
              </th>
              <th className="num" title="Median end-to-end latency">
                Latency p50
              </th>
              <th className="num" title="Tokens sent in the prompt">
                Tokens in
              </th>
              <th className="num" title="Tokens the model wrote back">
                Tokens out
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={10} className="table-empty">
                  No requests in this filter window yet.
                </td>
              </tr>
            ) : (
              rows.map((entry) => (
                <tr key={entry.key}>
                  <td className="mono stats-key" title={entry.key}>
                    {entry.key}
                  </td>
                  <td className="num">{formatNumber(entry.requests)}</td>
                  <td className="num">{formatUsd(entry.cost_usd)}</td>
                  <td className={entry.errors > 0 ? "num t-err" : "num"}>{formatNumber(entry.errors)}</td>
                  <td className="num">{formatMs(entry.ttft_p50_ms)}</td>
                  <td className="num">{formatTps(entry.tokens_per_sec_p50)}</td>
                  <td className="num">{formatNumber(entry.cache_read_tokens)}</td>
                  <td className="num">{formatMs(entry.p50_ms)}</td>
                  <td className="num">{formatNumber(entry.tokens_in)}</td>
                  <td className="num">{formatNumber(entry.tokens_out)}</td>
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}

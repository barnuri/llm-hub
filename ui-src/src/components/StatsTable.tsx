import type { StatsEntry } from "../lib/types";
import { formatMs, formatNumber, formatPct, formatUsd } from "../lib/format";
import { latencyHeat, rateHeat, ttftHeat } from "../lib/colors";
import { CompactCount } from "./CompactCount";
import { Heat } from "./Heat";
import { ModelName } from "./ModelName";
import { ModelTag } from "./ModelTag";
import { Panel } from "./Panel";
import { ShareBar } from "./ShareBar";

interface StatsTableProps {
  readonly label: string;
  readonly rows: readonly StatsEntry[];
}

const COLUMN_COUNT = 11;

function formatRate(tokensPerSec: number): string {
  if (!tokensPerSec) {
    return "—";
  }
  return formatNumber(tokensPerSec, tokensPerSec >= 100 ? 0 : 1);
}

function KeyCell({ entryKey }: { readonly entryKey: string }) {
  const slash = entryKey.indexOf("/");
  const name = slash > 0 ? entryKey.slice(slash + 1) : entryKey;
  return (
    <ModelTag colorKey={name}>
      <ModelName id={entryKey} />
    </ModelTag>
  );
}

export function StatsTable({ label, rows }: StatsTableProps) {
  const totalRequests = rows.reduce((sum, entry) => sum + entry.requests, 0);
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
              <th className="stats-share-col" title="Share of all requests in this table">
                Share
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
                <td colSpan={COLUMN_COUNT} className="table-empty">
                  No requests in this filter window yet.
                </td>
              </tr>
            ) : (
              rows.map((entry) => {
                const share = totalRequests > 0 ? entry.requests / totalRequests : 0;
                return (
                  <tr key={entry.key}>
                    <td className="mono stats-key" title={entry.key}>
                      <KeyCell entryKey={entry.key} />
                    </td>
                    <td className="num">{formatNumber(entry.requests)}</td>
                    <td>
                      <ShareBar fraction={share} label={`${formatPct(share * 100)} of requests`} />
                    </td>
                    <td className="num">{formatUsd(entry.cost_usd)}</td>
                    <td className={entry.errors > 0 ? "num t-err" : "num"}>{formatNumber(entry.errors)}</td>
                    <td className="num">
                      <Heat level={ttftHeat(entry.ttft_p50_ms)}>{formatMs(entry.ttft_p50_ms)}</Heat>
                    </td>
                    <td className="num">
                      <Heat level={rateHeat(entry.tokens_per_sec_p50)}>
                        {formatRate(entry.tokens_per_sec_p50)}
                      </Heat>
                    </td>
                    <td className="num">
                      <CompactCount value={entry.cache_read_tokens} />
                    </td>
                    <td className="num">
                      <Heat level={latencyHeat(entry.p50_ms)}>{formatMs(entry.p50_ms)}</Heat>
                    </td>
                    <td className="num">
                      <CompactCount value={entry.tokens_in} />
                    </td>
                    <td className="num">
                      <CompactCount value={entry.tokens_out} />
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}

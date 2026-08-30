import { useEffect, useState } from "react";

import { api } from "../lib/api";
import { formatTime } from "../lib/format";
import type { UsageReport } from "../lib/types";

export function UsageScreen() {
  const [report, setReport] = useState<UsageReport | null>(null);
  const [unavailable, setUnavailable] = useState(false);

  useEffect(() => {
    api<UsageReport>("/api/usage")
      .then(setReport)
      .catch(() => setUnavailable(true));
  }, []);

  if (unavailable) {
    return (
      <section className="tab active">
        <p className="note">
          Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to record usage history.
        </p>
      </section>
    );
  }

  const tiles: ReadonlyArray<readonly [string, number]> = [
    ["Requests", report?.total_requests ?? 0],
    ["Errors", report?.total_errors ?? 0],
    ["Tokens in", report?.total_tokens_in ?? 0],
    ["Tokens out", report?.total_tokens_out ?? 0],
  ];

  return (
    <section className="tab active">
      <div className="stat-tiles">
        {tiles.map(([label, value]) => (
          <div key={label} className="stat-tile">
            <div className="value">{value}</div>
            <div className="label">{label}</div>
          </div>
        ))}
      </div>
      <table className="table">
        <thead>
          <tr>
            <th>Time</th>
            <th>Model</th>
            <th className="num">Status</th>
            <th className="num">Latency ms</th>
            <th className="num">In</th>
            <th className="num">Out</th>
          </tr>
        </thead>
        <tbody>
          {(report?.recent ?? []).map((row, index) => (
            <tr key={`${row.ts_ms}-${index}`}>
              <td className="mono">{formatTime(row.ts_ms)}</td>
              <td className="mono">{row.model}</td>
              <td className="num">{row.status}</td>
              <td className="num">{row.latency_ms}</td>
              <td className="num">{row.tokens_in}</td>
              <td className="num">{row.tokens_out}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

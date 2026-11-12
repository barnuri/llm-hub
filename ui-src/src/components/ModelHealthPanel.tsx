import { useEffect, useState } from "react";

import { api } from "../lib/api";
import { formatMs, formatNumber } from "../lib/format";
import { useLive } from "../lib/live";
import type { ModelHealthRow, ModelHealthSnapshot } from "../lib/types";
import { ModelName } from "./ModelName";
import { Panel } from "./Panel";
import { Pill } from "./Pill";

const UNSET = "—";
const PERCENT = 100;
const MS_PER_SECOND = 1000;

function failureText(row: ModelHealthRow): string {
  const failure = row.last_failure;
  if (failure === null) {
    return UNSET;
  }
  return typeof failure === "string" ? failure : `HTTP ${failure.status}`;
}

function StatusPill({ row }: { readonly row: ModelHealthRow }) {
  if (row.cooled_for_ms > 0) {
    const seconds = Math.ceil(row.cooled_for_ms / MS_PER_SECOND);
    return (
      <Pill tone="danger" title="Smart Auto skips this model until the wait ends">
        Skipped {seconds} s
      </Pill>
    );
  }
  return <Pill tone="ok">In pool</Pill>;
}

/** The models Smart Auto skips after a failure, and the error rate and speed it ranks the rest by. */
export function ModelHealthPanel() {
  const { tick } = useLive();
  const [snapshot, setSnapshot] = useState<ModelHealthSnapshot | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    api<ModelHealthSnapshot>("/api/smart-auto/health")
      .then((next) => {
        if (!cancelled) {
          setSnapshot(next);
          setFailed(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setFailed(true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [tick]);

  const rows = snapshot?.models ?? [];
  const skipped = rows.filter((row) => row.cooled_for_ms > 0).length;

  let body;
  if (failed) {
    body = (
      <p className="note" role="status">
        Model health is unavailable. Check that the health endpoint responds.
      </p>
    );
  } else {
    body = (
      <div className="table-scroll smart-auto-table">
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              <th>Route</th>
              <th>Status</th>
              <th>Last failure</th>
              <th className="num">Errors</th>
              <th className="num">First byte</th>
              <th className="num">Speed</th>
              <th className="num">Cost</th>
            </tr>
          </thead>
          <tbody>
            {snapshot === null ? (
              <tr>
                <td colSpan={8} className="table-empty">
                  Loading…
                </td>
              </tr>
            ) : rows.length === 0 ? (
              <tr>
                <td colSpan={8} className="table-empty">
                  No attempts since the hub started. Marks are kept in memory and clear on restart.
                </td>
              </tr>
            ) : (
              rows.map((row) => {
                const cost = snapshot.costs[row.model];
                return (
                  <tr key={`${row.model}|${row.endpoint}`}>
                    <td>
                      <ModelName id={row.model} />
                    </td>
                    <td className="dim">{row.endpoint}</td>
                    <td>
                      <StatusPill row={row} />
                    </td>
                    <td>{failureText(row)}</td>
                    <td className="num">
                      {row.outcomes === 0 ? UNSET : `${formatNumber(row.error_rate * PERCENT, 0)}% of ${row.outcomes}`}
                    </td>
                    <td className="num">{row.ttft_ms === null ? UNSET : formatMs(row.ttft_ms)}</td>
                    <td className="num">{row.speed === null ? UNSET : `${row.speed}/10`}</td>
                    <td className="num">{cost === undefined ? UNSET : `${cost}/10`}</td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
    );
  }

  return (
    <Panel
      title="Model health"
      sub={`${formatNumber(rows.length)} tracked, ${formatNumber(skipped)} skipped`}
      className="smart-auto-panel"
    >
      {body}
      <p className="dim models-side-note">
        A failed attempt skips the model for 30 s, doubling to 10 min; an answer clears it. Speed and cost (10 is best)
        and the error rate feed the ranking.
      </p>
    </Panel>
  );
}

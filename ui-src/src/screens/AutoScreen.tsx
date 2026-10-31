import { useEffect, useState } from "react";

import { api } from "../lib/api";
import { formatDateTime, formatNumber } from "../lib/format";
import { setQueryParam, useRoute } from "../lib/router";
import type { AutoDecision, AutoDecisionsResponse, AutoTurn } from "../lib/types";

const REFRESH_MS = 5000;
const LIMIT = 100;
const SESSION_PREFIX = 8;

const TURN_CLASS: Readonly<Record<AutoTurn, string>> = {
  routed: "badge on",
  reused: "badge",
  default: "badge warn",
};

const TURN_HELP: Readonly<Record<AutoTurn, string>> = {
  routed: "Copilot Auto picked the model on this turn",
  reused: "The conversation's earlier Auto pick served this turn",
  default: "Auto did not answer, so the default model served this turn",
};

export function AutoScreen() {
  const route = useRoute();
  const session = route.query.get("session") ?? "";
  const [decisions, setDecisions] = useState<readonly AutoDecision[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);

  useEffect(() => {
    let cancelled = false;
    const load = () => {
      const params = new URLSearchParams({ limit: String(LIMIT) });
      if (session) {
        params.set("session", session);
      }
      api<AutoDecisionsResponse>(`/api/copilot/auto/decisions?${params}`)
        .then((body) => {
          if (!cancelled) {
            setDecisions(body.decisions);
            setUnavailable(false);
          }
        })
        .catch(() => {
          if (!cancelled) {
            setUnavailable(true);
          }
        });
    };
    load();
    const timer = setInterval(load, REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [session]);

  if (unavailable && decisions === null) {
    return (
      <section className="tab active">
        <h2>Copilot Auto</h2>
        <p className="note" role="status">
          Auto decisions are unavailable. Add a Copilot profile and send a request to <code>copilot/auto</code>.
        </p>
      </section>
    );
  }

  const rows = decisions ?? [];

  return (
    <section className="tab active">
      <h2>Copilot Auto</h2>
      <p className="dim">The model Copilot Auto chose for each recent turn, newest first. Refreshes every 5 s.</p>
      <div className="toolbar">
        <input
          className="input search"
          type="search"
          placeholder="Filter by session id…"
          aria-label="Filter by session id"
          value={session}
          onChange={(event) => setQueryParam("session", event.target.value.trim())}
        />
        {session ? (
          <button type="button" className="btn small" onClick={() => setQueryParam("session", null)}>
            All sessions
          </button>
        ) : null}
        <span className="dim">
          {formatNumber(rows.length)} decision{rows.length === 1 ? "" : "s"}
        </span>
        {unavailable ? (
          <span className="dim" role="status">
            Refresh failed, showing the last result.
          </span>
        ) : null}
      </div>
      <div className="table-scroll">
        <table className="table">
          <thead>
            <tr>
              <th>Time</th>
              <th>Session</th>
              <th>Auto pick</th>
              <th>Model used</th>
              <th>Swap reason</th>
              <th>Complexity</th>
              <th>Turn</th>
            </tr>
          </thead>
          <tbody>
            {rows.length === 0 ? (
              <tr>
                <td colSpan={7} className="dim">
                  {decisions === null ? "Loading…" : "No Auto decisions yet."}
                </td>
              </tr>
            ) : (
              rows.map((row) => (
                <tr key={`${row.ts_ms}-${row.session ?? ""}-${row.model}-${row.turn}`}>
                  <td className="mono nowrap">{formatDateTime(row.ts_ms)}</td>
                  <td className="mono nowrap">
                    {row.session ? (
                      <button
                        type="button"
                        className="chip"
                        title={`${row.session}. Show only this session`}
                        onClick={() => setQueryParam("session", row.session)}
                      >
                        {row.session.slice(0, SESSION_PREFIX)}
                      </button>
                    ) : (
                      <span className="dim">none</span>
                    )}
                  </td>
                  <td className="mono nowrap" title={row.selection_reason ?? undefined}>
                    {row.auto_pick}
                  </td>
                  <td className="mono nowrap">{row.model}</td>
                  <td className="error-reason">{row.swap_reason ?? <span className="dim">none</span>}</td>
                  <td className="mono nowrap">{row.complexity ?? <span className="dim">unknown</span>}</td>
                  <td className="nowrap">
                    <span className={`auto-turn ${TURN_CLASS[row.turn]}`} title={TURN_HELP[row.turn]}>
                      {row.turn}
                    </span>
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

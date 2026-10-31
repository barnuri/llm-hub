import { useEffect, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { api } from "../lib/api";
import { formatDateTime, formatNumber } from "../lib/format";
import { setQueryParam, useRoute } from "../lib/router";
import type { AutoDecision, AutoDecisionsResponse, AutoTurn } from "../lib/types";
import "../styles/screens/auto.css";

const REFRESH_MS = 5000;
const LIMIT = 100;
const SESSION_PREFIX = 8;

const TURN_TONE: Readonly<Record<AutoTurn, PillTone>> = {
  routed: "ok",
  reused: "neutral",
  default: "warn",
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
      <section className="tab active auto">
        <Panel title="Auto decisions unavailable">
          <p className="note" role="status">
            Auto decisions are unavailable. Add a Copilot profile and send a request to <code>copilot/auto</code>.
          </p>
        </Panel>
      </section>
    );
  }

  const rows = decisions ?? [];
  const turnCount = (turn: AutoTurn) => rows.filter((row) => row.turn === turn).length;
  const countValue = (value: number) => (decisions === null ? "…" : formatNumber(value));
  const defaults = turnCount("default");

  return (
    <section className="tab active auto" aria-busy={decisions === null}>
      <p className="dim auto-intro">
        The model Copilot Auto chose for each recent turn, newest first. Refreshes every 5 s.
      </p>
      <div className="kpis">
        <KpiCard label="Decisions" value={countValue(rows.length)} sub={session ? "This session" : `Last ${LIMIT}`} />
        <KpiCard label="Routed" value={countValue(turnCount("routed"))} sub={TURN_HELP.routed} />
        <KpiCard label="Reused" value={countValue(turnCount("reused"))} sub={TURN_HELP.reused} tone="info" />
        <KpiCard
          label="Fell back to default"
          value={countValue(defaults)}
          sub={TURN_HELP.default}
          tone={defaults > 0 ? "warn" : "default"}
        />
      </div>

      <Panel
        title="Recent decisions"
        sub={`${formatNumber(rows.length)} decision${rows.length === 1 ? "" : "s"}`}
        actions={
          unavailable ? (
            <Pill tone="warn" dot>
              <span role="status">Refresh failed, showing the last result</span>
            </Pill>
          ) : null
        }
      >
        <div className="toolbar">
          <label className="auto-filter">
            <span>Session</span>
            <input
              className="input search"
              type="search"
              placeholder="Filter by session id…"
              value={session}
              onChange={(event) => setQueryParam("session", event.target.value.trim())}
            />
          </label>
          {session ? (
            <button type="button" className="btn small" onClick={() => setQueryParam("session", null)}>
              All sessions
            </button>
          ) : null}
        </div>
        <div className="table-scroll auto-table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Session</th>
                <th>Tier</th>
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
                  <td colSpan={8} className="dim auto-empty">
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
                    <td className="mono nowrap">{row.tier}</td>
                    <td className="mono nowrap" title={row.selection_reason ?? undefined}>
                      {row.auto_pick}
                    </td>
                    <td className="mono nowrap">{row.model}</td>
                    <td className="error-reason">{row.swap_reason ?? <span className="dim">none</span>}</td>
                    <td className="mono nowrap">{row.complexity ?? <span className="dim">unknown</span>}</td>
                    <td className="nowrap">
                      <Pill tone={TURN_TONE[row.turn]} title={TURN_HELP[row.turn]}>
                        {row.turn}
                      </Pill>
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>
    </section>
  );
}

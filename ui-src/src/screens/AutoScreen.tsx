import { useEffect, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { MixTable, type MixRow } from "../components/MixTable";
import { ModelName } from "../components/ModelName";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { api } from "../lib/api";
import { formatDateTime, formatNumber } from "../lib/format";
import { setQueryParam, useRoute } from "../lib/router";
import type { AutoDecision, AutoDecisionsResponse, AutoTurn, HubModel } from "../lib/types";
import "../styles/screens/auto.css";

const REFRESH_MS = 5000;
const LIMIT = 100;
const SESSION_PREFIX = 8;
const SPARK_SLICES = 7;
const MODEL_MIX_ROWS = 4;
const UNKNOWN_COMPLEXITY = "unknown";

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

/** Turns per equal time slice, oldest first, across the span the shown decisions cover. */
function sliceCounts(rows: readonly AutoDecision[], turn: AutoTurn | null): number[] | undefined {
  if (rows.length < 2) {
    return undefined;
  }
  const times = rows.map((row) => row.ts_ms);
  const oldest = Math.min(...times);
  const span = Math.max(...times) - oldest;
  if (span <= 0) {
    return undefined;
  }
  const counts = Array.from({ length: SPARK_SLICES }, () => 0);
  for (const row of rows) {
    if (turn === null || row.turn === turn) {
      const slice = Math.min(SPARK_SLICES - 1, Math.floor(((row.ts_ms - oldest) / span) * SPARK_SLICES));
      counts[slice] = (counts[slice] ?? 0) + 1;
    }
  }
  return counts;
}

function mixRows(rows: readonly AutoDecision[], keyOf: (row: AutoDecision) => string, limit?: number): MixRow[] {
  const counts = new Map<string, number>();
  for (const row of rows) {
    const key = keyOf(row);
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  const ranked = [...counts.entries()].sort(
    ([leftKey, left], [rightKey, right]) => right - left || leftKey.localeCompare(rightKey),
  );
  const shown = limit === undefined || ranked.length <= limit ? ranked : ranked.slice(0, limit - 1);
  const restCount = ranked.slice(shown.length).reduce((sum, [, count]) => sum + count, 0);
  const entries = restCount > 0 ? [...shown, [`${ranked.length - shown.length} others`, restCount] as const] : shown;
  return entries.map(([key, count]) => ({
    key,
    label: key,
    count,
    fillPct: rows.length > 0 ? (count / rows.length) * 100 : 0,
    tone: key === UNKNOWN_COMPLEXITY ? "off" : undefined,
  }));
}

export function AutoScreen({ models }: { readonly models: readonly HubModel[] }) {
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
            Auto decisions unavailable: add a Copilot profile and send a request to <code>copilot/auto</code>.
          </p>
        </Panel>
      </section>
    );
  }

  const rows = decisions ?? [];
  const turnCount = (turn: AutoTurn) => rows.filter((row) => row.turn === turn).length;
  const countValue = (value: number) => (decisions === null ? "…" : formatNumber(value));
  const defaults = turnCount("default");
  const sessionCount = new Set(rows.flatMap((row) => (row.session ? [row.session] : []))).size;
  const windowLabel = session ? "this session" : `last ${LIMIT}`;
  const decisionsSub = session
    ? "This session"
    : `Last ${LIMIT} · ${formatNumber(sessionCount)} session${sessionCount === 1 ? "" : "s"}`;
  const seriesLabel = (label: string) => `${label} across ${SPARK_SLICES} equal time slices of the shown decisions`;

  return (
    <section className="tab active auto" aria-busy={decisions === null}>
      <div className="auto-head">
        <span className="dim">The model Copilot Auto chose for each recent turn, newest first.</span>
        {unavailable ? (
          <Pill tone="warn" dot>
            <span role="status">Refresh failed, showing the last result</span>
          </Pill>
        ) : (
          <Pill tone="ok" dot>
            live · every {REFRESH_MS / 1000} s
          </Pill>
        )}
      </div>

      <p className="note">
        Auto usually reuses a model during a conversation. A scheduled complexity check or token refresh can select a new model.
        The hub can also substitute a model when the selected one cannot handle a request. <a href="https://docs.github.com/en/copilot/concepts/models/auto-model-selection" target="_blank" rel="noreferrer">Copilot tier guidance</a>
      </p>
      <Panel title="Auto tiers" sub="Choose a routing preference">
        <div className="auto-tier-guide">
          {models.filter((model) => model.auto_tier !== undefined).map((model) => (
            <article className="auto-tier-row" key={model.id}>
              <div className="auto-tier-name">{model.display_name ?? model.id}</div>
              <p>{model.description}</p>
            </article>
          ))}
        </div>
      </Panel>

      <div className="auto-kpis">
        <KpiCard
          label="Decisions"
          value={countValue(rows.length)}
          sub={decisionsSub}
          series={sliceCounts(rows, null)}
          seriesLabel={seriesLabel("Decisions")}
        />
        <KpiCard
          label="Routed"
          value={countValue(turnCount("routed"))}
          sub={TURN_HELP.routed}
          series={sliceCounts(rows, "routed")}
          seriesLabel={seriesLabel("Routed turns")}
        />
        <KpiCard
          label="Reused"
          value={countValue(turnCount("reused"))}
          sub={TURN_HELP.reused}
          series={sliceCounts(rows, "reused")}
          seriesLabel={seriesLabel("Reused turns")}
        />
        <KpiCard
          label="Fell back to default"
          value={countValue(defaults)}
          sub={TURN_HELP.default}
          series={sliceCounts(rows, "default")}
          seriesLabel={seriesLabel("Default turns")}
          tone={defaults > 0 ? "warn" : "default"}
        />
      </div>

      {rows.length > 0 ? (
        <div className="auto-mix">
          <Panel title="Tier mix" sub={windowLabel}>
            <MixTable labelHeading="Tier" countHeading="Turns" rows={mixRows(rows, (row) => row.tier)} />
          </Panel>
          <Panel title="Complexity mix" sub="from Auto's selection reason">
            <MixTable
              labelHeading="Complexity"
              countHeading="Turns"
              rows={mixRows(rows, (row) => row.complexity ?? UNKNOWN_COMPLEXITY)}
            />
          </Panel>
          <Panel title="Model used" sub="after swaps">
            <MixTable
              labelHeading="Model"
              countHeading="Turns"
              rows={mixRows(rows, (row) => row.model, MODEL_MIX_ROWS)}
            />
          </Panel>
        </div>
      ) : null}

      <Panel
        className="auto-decisions"
        title="Recent decisions"
        sub={`${formatNumber(rows.length)} decision${rows.length === 1 ? "" : "s"}`}
        actions={
          <div className="auto-filter">
            <label className="auto-filter-label" htmlFor="auto-session">
              Session
            </label>
            <input
              id="auto-session"
              className="input search auto-session"
              type="search"
              placeholder="Filter by session id…"
              value={session}
              onChange={(event) => setQueryParam("session", event.target.value.trim())}
            />
            {session ? (
              <button type="button" className="btn small" onClick={() => setQueryParam("session", null)}>
                All sessions
              </button>
            ) : null}
          </div>
        }
      >
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
                <th className="auto-turn">Turn</th>
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
                    <td className="mono nowrap dim">{formatDateTime(row.ts_ms)}</td>
                    <td className="mono nowrap">
                      {row.session ? (
                        <button
                          type="button"
                          className="chip auto-session-chip"
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
                    <td className="auto-pick" title={row.selection_reason ?? undefined}>
                      <span className="mono nowrap">{row.auto_pick}</span>
                      {row.selection_reason ? <span className="auto-reason">{row.selection_reason}</span> : null}
                    </td>
                    <td className="mono nowrap"><ModelName id={row.model} /></td>
                    <td className="error-reason">{row.swap_reason ?? <span className="dim">none</span>}</td>
                    <td className="mono nowrap">
                      {row.complexity ?? <span className="dim">{UNKNOWN_COMPLEXITY}</span>}
                    </td>
                    <td className="nowrap auto-turn">
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
        <div className="auto-foot">
          <span>Click a session id to show only that session. Hover a pick for the full selection reason.</span>
          <span className="mono">{formatNumber(rows.length)} shown</span>
        </div>
      </Panel>
    </section>
  );
}

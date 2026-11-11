import { useEffect, useState } from "react";

import { Panel } from "../components/Panel";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { shortModel } from "../components/UsageModelChart";
import { errorText, formatDateTime, formatNumber, formatPct, formatUsd } from "../lib/format";
import { useLive } from "../lib/live";
import { navigate, useRoute } from "../lib/router";
import {
  fetchSessions,
  formatDuration,
  parseSessionsRange,
  type SessionList,
  type SessionsRange,
  type SessionSummary,
  sessionName,
  workerFromPath,
} from "../lib/sessions";
import "../styles/screens/sessions.css";
import { SessionDetailPage } from "./SessionDetailPage";

const RANGE_OPTIONS: readonly SegmentedOption<SessionsRange>[] = [
  { value: "today", label: "Today" },
  { value: "1d", label: "24 h" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "all", label: "All time" },
];

const LIST_COLUMN_COUNT = 15;

export function SessionsScreen() {
  const route = useRoute();
  const range = parseSessionsRange(route.query.get("range"));
  const worker = workerFromPath(route.path);
  if (worker !== null) {
    return <SessionDetailPage worker={worker} range={range} />;
  }
  return <SessionsList range={range} />;
}

function promptCell(value: number | undefined): string {
  return value === undefined ? "—" : formatNumber(value);
}

function SessionsList({ range }: { readonly range: SessionsRange }) {
  const [list, setList] = useState<SessionList | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const { tick } = useLive();

  useEffect(() => {
    let cancelled = false;
    fetchSessions(range)
      .then((body) => {
        if (!cancelled) {
          setUnavailable(null);
          setList(body);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setList(null);
          setUnavailable(errorText(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [range, tick]);

  const rows = [...(list?.sessions ?? [])].sort((left, right) => right.cost_usd - left.cost_usd);

  return (
    <section className="tab active sessions" aria-busy={list === null && !unavailable}>
      <div className="toolbar sessions-toolbar">
        <Segmented
          label="Sessions period"
          options={RANGE_OPTIONS}
          value={range}
          onChange={(value) => navigate(`/sessions?range=${value}`)}
        />
      </div>
      <p className="dim sessions-intro">
        Sessions grouped by worker. Requests count every row. Tokens, prompt sizes and cost leave out failed requests
        (status 400 and above).
      </p>

      {unavailable ? (
        <Panel title="Sessions unavailable">
          <p className="note" role="status">
            {unavailable}
          </p>
        </Panel>
      ) : null}

      {list ? (
        <Panel title="Sessions" sub={`${formatNumber(rows.length)} sessions, most expensive first`}>
          <div className="table-scroll sessions-table-scroll">
            <table className="table sessions-table">
              <thead>
                <tr>
                  <th>Project</th>
                  <th>Client</th>
                  <th className="num">Duration</th>
                  <th className="num">Requests</th>
                  <th className="num">User turns</th>
                  <th>Models</th>
                  <th className="num">Prompt median</th>
                  <th className="num">Prompt p90</th>
                  <th className="num">Prompt peak</th>
                  <th className="num">Cost</th>
                  <th className="num">Cache read</th>
                  <th className="num">Cache write</th>
                  <th className="num" title="Cache writes after the cache expired">
                    Idle cache write
                  </th>
                  <th className="num">Long tier</th>
                  <th className="num">Compactions</th>
                </tr>
              </thead>
              <tbody>
                {rows.length === 0 ? (
                  <tr>
                    <td colSpan={LIST_COLUMN_COUNT} className="note sessions-empty">
                      No sessions in this window yet.
                    </td>
                  </tr>
                ) : null}
                {rows.map((session) => (
                  <SessionRow key={session.worker} session={session} range={range} />
                ))}
                <tr className="sessions-note-row">
                  <td colSpan={LIST_COLUMN_COUNT}>
                    {formatNumber(list.no_session_id.requests)} cache-write requests had no session id and are not
                    grouped into a session ({formatNumber(list.no_session_id.cache_write_tokens)} tokens,{" "}
                    {formatUsd(list.no_session_id.cache_write_usd)} cache write).
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </Panel>
      ) : null}
    </section>
  );
}

function SessionRow({ session, range }: { readonly session: SessionSummary; readonly range: SessionsRange }) {
  const modelNames = session.models.map(shortModel).join(", ");
  const prompt = session.prompt;
  return (
    <tr>
      <td>
        <button
          type="button"
          className="sessions-name"
          title={session.worker}
          onClick={() => navigate(`/sessions/${encodeURIComponent(session.worker)}?range=${range}`)}
        >
          {sessionName(session)}
        </button>
      </td>
      <td className="nowrap">{session.client ?? "—"}</td>
      <td className="num nowrap">{formatDuration(session.duration_ms)}</td>
      <td className="num">{formatNumber(session.requests)}</td>
      <td className="num">{formatNumber(session.user_turns)}</td>
      <td className="sessions-models mono" title={modelNames}>
        {modelNames || "—"}
      </td>
      <td className="num">{promptCell(prompt?.median)}</td>
      <td className="num">{promptCell(prompt?.p90)}</td>
      <td className="num" title={prompt ? formatDateTime(prompt.peak_ts_ms) : undefined}>
        {promptCell(prompt?.max)}
      </td>
      <td className="num nowrap">{formatUsd(session.cost_usd)}</td>
      <td className="num nowrap">{formatUsd(session.cache_read_usd)}</td>
      <td className="num nowrap">{formatUsd(session.cache_write_usd)}</td>
      <td className="num nowrap">{formatUsd(session.idle_cache_write_usd)}</td>
      <td className="num">{formatPct(session.long_tier_share * 100)}</td>
      <td className="num">{formatNumber(session.compactions)}</td>
    </tr>
  );
}

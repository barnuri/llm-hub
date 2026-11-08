import { useEffect, useMemo, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { api } from "../lib/api";
import { formatDateTime, formatMs, formatNumber, formatUsd } from "../lib/format";
import { classifyEvents, COST_KIND_TEXT, type CostKind, eventCost, sumCost, WASTE_KINDS } from "../lib/smartAutoCost";
import { setQueryParam } from "../lib/router";
import type { HubModelPrice, SelectionEvent, SmartAutoAttemptText } from "../lib/types";

const MAX_TURNS = 100;
const SHORT_KEY_CHARS = 8;
const UNSET = "—";

/** What each route reason code means, in the words the trace shows. */
const REASON_TEXT: Readonly<Record<string, string>> = {
  pinned: "Stayed on the model this conversation already uses. The role did not change materially.",
  tool_continuation_replay: "A tool result continued the turn on the model that made the tool call.",
  incumbent: "Returned to a role and reused the model that role used before.",
  role_switch: "The role changed materially. Picked the best model for the new role.",
  fresh: "No pin or incumbent existed. Picked the best model for the role.",
};

const KIND_TONE: Readonly<Record<CostKind, PillTone>> = {
  answer: "ok",
  checker: "neutral",
  repair: "warn",
  failed: "danger",
  rejected_draft: "warn",
};

interface TurnSummary {
  readonly turn: string;
  readonly events: readonly SelectionEvent[];
  readonly first: SelectionEvent;
  readonly models: readonly string[];
  readonly attempts: number;
  readonly costUsd: number;
  readonly wasteUsd: number;
}

function shortKey(key: string | null): string {
  return key === null ? UNSET : key.slice(0, SHORT_KEY_CHARS);
}

function groupTurns(events: readonly SelectionEvent[], prices: ReadonlyMap<string, HubModelPrice>): TurnSummary[] {
  const byTurn = new Map<string, SelectionEvent[]>();
  for (const event of events) {
    if (event.turn === null) {
      continue;
    }
    const list = byTurn.get(event.turn) ?? [];
    list.push(event);
    byTurn.set(event.turn, list);
  }
  const turns: TurnSummary[] = [];
  for (const [turn, turnEvents] of byTurn) {
    const first = turnEvents[0];
    if (first === undefined) {
      continue;
    }
    const kinds = classifyEvents(turnEvents);
    let wasteUsd = 0;
    for (const event of turnEvents) {
      const kind = kinds.get(event);
      const cost = eventCost(event, prices);
      if (cost !== null && kind !== undefined && WASTE_KINDS.includes(kind)) {
        wasteUsd += cost;
      }
    }
    turns.push({
      turn,
      events: turnEvents,
      first,
      models: [...new Set(turnEvents.filter((event) => event.stage === "solver").map((event) => event.model))],
      attempts: new Set(turnEvents.flatMap((event) => (event.attempt ? [event.attempt] : []))).size,
      costUsd: sumCost(turnEvents, prices).usd,
      wasteUsd,
    });
  }
  return turns.reverse().slice(0, MAX_TURNS);
}

function TurnList({
  turns,
  selected,
  onSelect,
}: {
  readonly turns: readonly TurnSummary[];
  readonly selected: string | null;
  readonly onSelect: (turn: string) => void;
}) {
  if (turns.length === 0) {
    return <p className="table-empty">No turns yet. Requests to hub/smart-auto-balance appear here as they run.</p>;
  }
  return (
    <div className="table-scroll">
      <table className="table smart-auto-turns">
        <caption className="sr-only">Turns, newest first. Select one to see its trace.</caption>
        <thead>
          <tr>
            <th>Time</th>
            <th>Turn</th>
            <th>Role</th>
            <th>Reason</th>
            <th>Model</th>
            <th className="num">Calls</th>
            <th className="num">Cost</th>
            <th className="num">Waste</th>
          </tr>
        </thead>
        <tbody>
          {turns.map((turn) => (
            <tr key={turn.turn} className={turn.turn === selected ? "active" : undefined}>
              <td data-label="Time">
                <button type="button" className="link-button" onClick={() => onSelect(turn.turn)}>
                  {formatDateTime(turn.first.ts_ms)}
                </button>
              </td>
              <td className="mono" title={turn.turn} data-label="Turn">
                {shortKey(turn.turn)}
              </td>
              <td data-label="Role">{turn.first.role ?? UNSET}</td>
              <td className="smart-auto-reason" data-label="Reason">
                {turn.first.reason ?? UNSET}
              </td>
              <td className="mono" data-label="Model">
                {turn.models.join(", ") || UNSET}
              </td>
              <td className="num" data-label="Calls">
                {turn.events.length}
              </td>
              <td className="num" data-label="Cost">
                {turn.costUsd > 0 ? formatUsd(turn.costUsd) : UNSET}
              </td>
              <td className="num" data-label="Waste">
                {turn.wasteUsd > 0 ? formatUsd(turn.wasteUsd) : UNSET}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function AttemptText({ attempt }: { readonly attempt: string }) {
  const [state, setState] = useState<{ readonly data: SmartAutoAttemptText | null; readonly error: boolean } | null>(
    null,
  );
  // The parent remounts this component for each attempt, so the state starts empty each time.
  useEffect(() => {
    let cancelled = false;
    api<SmartAutoAttemptText>(`/api/smart-auto/attempt?attempt=${encodeURIComponent(attempt)}`)
      .then((data) => {
        if (!cancelled) {
          setState({ data, error: false });
        }
      })
      .catch(() => {
        if (!cancelled) {
          setState({ data: null, error: true });
        }
      });
    return () => {
      cancelled = true;
    };
  }, [attempt]);

  if (state === null) {
    return <p className="note">Loading the stored text…</p>;
  }
  if (state.error) {
    return <p className="note" role="status">The stored text cannot be read. It needs the sqlite store.</p>;
  }
  if (state.data === null || (state.data.prompt === null && state.data.answer === null)) {
    return <p className="note">No text was kept for this attempt.</p>;
  }
  return (
    <div className="smart-auto-texts">
      <h4>Prompt ({formatNumber(state.data.prompt?.length ?? 0)} chars)</h4>
      <pre className="smart-auto-text">{state.data.prompt ?? ""}</pre>
      <h4>Answer ({formatNumber(state.data.answer?.length ?? 0)} chars)</h4>
      <pre className="smart-auto-text">{state.data.answer || "No answer text was kept."}</pre>
    </div>
  );
}

function TurnDetail({
  turn,
  prices,
}: {
  readonly turn: TurnSummary;
  readonly prices: ReadonlyMap<string, HubModelPrice>;
}) {
  const kinds = useMemo(() => classifyEvents(turn.events), [turn.events]);
  const [openAttempt, setOpenAttempt] = useState<string | null>(null);
  const first = turn.first;
  const planBacked = first.plan_backed === undefined || first.plan_backed === null ? UNSET : first.plan_backed ? "Yes" : "No";
  const reasonCode = first.reason ?? UNSET;
  const wasteCalls = turn.events.filter((event) => WASTE_KINDS.includes(kinds.get(event) ?? "answer")).length;

  return (
    <Panel
      title={`Turn ${shortKey(turn.turn)}`}
      sub={`${formatDateTime(first.ts_ms)} · ${turn.events.length} calls · ${wasteCalls} waste`}
      className="smart-auto-panel"
    >
      <dl className="smart-auto-why">
        <div>
          <dt>Tier</dt>
          <dd>{first.tier ?? UNSET}</dd>
        </div>
        <div>
          <dt>Selector</dt>
          <dd className="mono">{first.selector ?? UNSET}</dd>
        </div>
        <div>
          <dt>Role</dt>
          <dd>{first.role ?? UNSET}</dd>
        </div>
        <div>
          <dt>Plan-backed</dt>
          <dd>{planBacked}</dd>
        </div>
        <div>
          <dt>Route reason</dt>
          <dd>
            <span className="mono">{reasonCode}</span>
            {first.reason !== null && REASON_TEXT[first.reason] !== undefined ? <p className="note">{REASON_TEXT[first.reason]}</p> : null}
          </dd>
        </div>
        <div>
          <dt>Effort</dt>
          <dd className="mono">{first.selected_effort ?? first.sent_effort ?? UNSET}</dd>
        </div>
      </dl>

      <div className="table-scroll">
        <table className="table smart-auto-calls">
          <caption className="sr-only">Every call in this turn, in order.</caption>
          <thead>
            <tr>
              <th>Time</th>
              <th>Attempt</th>
              <th>Stage</th>
              <th>Model</th>
              <th>State</th>
              <th>Kind</th>
              <th className="num">Status</th>
              <th className="num">Latency</th>
              <th className="num">Tokens in / out</th>
              <th className="num">Cost</th>
              <th>Text</th>
            </tr>
          </thead>
          <tbody>
            {turn.events.map((event) => {
              const kind = kinds.get(event) ?? "answer";
              const cost = eventCost(event, prices);
              const showText = event.attempt !== null && event.stage === "solver" && event.state === "selected";
              return (
                <tr key={event.seq}>
                  <td data-label="Time">{formatDateTime(event.ts_ms)}</td>
                  <td className="mono" title={event.attempt ?? undefined} data-label="Attempt">
                    {shortKey(event.attempt)}
                  </td>
                  <td data-label="Stage">{event.stage}</td>
                  <td className="mono" data-label="Model">
                    {event.model}
                  </td>
                  <td data-label="State">{event.state}</td>
                  <td data-label="Kind">
                    <Pill tone={KIND_TONE[kind]}>{COST_KIND_TEXT[kind]}</Pill>
                  </td>
                  <td className="num" data-label="Status">
                    {event.status ?? UNSET}
                  </td>
                  <td className="num" data-label="Latency">
                    {event.latency_ms === null ? UNSET : formatMs(event.latency_ms)}
                  </td>
                  <td className="num" data-label="Tokens in / out">
                    {event.tokens_in === null && event.tokens_out === null
                      ? UNSET
                      : `${formatNumber(event.tokens_in ?? 0)} / ${formatNumber(event.tokens_out ?? 0)}`}
                  </td>
                  <td className="num" data-label="Cost">
                    {cost === null ? UNSET : formatUsd(cost)}
                  </td>
                  <td data-label="Text">
                    {showText && event.attempt !== null ? (
                      <button
                        type="button"
                        className="link-button"
                        aria-expanded={openAttempt === event.attempt}
                        onClick={() => setOpenAttempt(openAttempt === event.attempt ? null : event.attempt)}
                      >
                        {openAttempt === event.attempt ? "Hide" : "Show"}
                      </button>
                    ) : (
                      UNSET
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {openAttempt !== null ? <AttemptText key={openAttempt} attempt={openAttempt} /> : null}
    </Panel>
  );
}

interface SmartAutoTracePageProps {
  readonly events: readonly SelectionEvent[];
  readonly prices: ReadonlyMap<string, HubModelPrice>;
  /** The turn named in `?turn=`, if any. */
  readonly selectedTurn: string | null;
}

/** Every turn Smart Auto routed, with its reason, its calls, their cost and what was wasted. */
export function SmartAutoTracePage({ events, prices, selectedTurn }: SmartAutoTracePageProps) {
  const turns = useMemo(() => groupTurns(events, prices), [events, prices]);
  const detail = selectedTurn === null ? undefined : turns.find((turn) => turn.turn === selectedTurn);
  const totals = sumCost(events, prices);
  const kinds = useMemo(() => classifyEvents(events), [events]);
  const waste = events.reduce((sum, event) => {
    const kind = kinds.get(event);
    const cost = eventCost(event, prices);
    return kind !== undefined && WASTE_KINDS.includes(kind) && cost !== null ? sum + cost : sum;
  }, 0);

  return (
    <>
      <div className="smart-auto-kpis">
        <KpiCard label="Turns" value={formatNumber(turns.length)} sub={`Newest ${MAX_TURNS} shown`} />
        <KpiCard label="Calls" value={formatNumber(events.length)} sub="Events in this tab" />
        <KpiCard label="Cost" value={formatUsd(totals.usd)} sub={`${formatNumber(totals.unpriced)} calls unpriced`} />
        <KpiCard
          label="Waste"
          value={formatUsd(waste)}
          sub={totals.usd > 0 ? `${((waste / totals.usd) * 100).toFixed(2)}% of cost` : "No cost yet"}
          tone={waste > 0 ? "danger" : "default"}
        />
      </div>
      <Panel title="Turns" sub="Select a turn to see its trace" className="smart-auto-panel">
        <TurnList turns={turns} selected={selectedTurn} onSelect={(turn) => setQueryParam("turn", turn)} />
      </Panel>
      {detail !== undefined ? (
        <TurnDetail turn={detail} prices={prices} />
      ) : selectedTurn !== null ? (
        <Panel title="Turn not in view">
          <p className="note" role="status">
            This turn is older than the events this tab holds, or it has no events yet.
          </p>
        </Panel>
      ) : null}
    </>
  );
}

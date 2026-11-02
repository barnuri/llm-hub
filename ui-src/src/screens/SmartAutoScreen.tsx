import { useEffect, useRef, useState } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { api } from "../lib/api";
import { formatDateTime, formatMs, formatNumber } from "../lib/format";
import type { SelectionEvent, SelectionHistoryResponse, SelectionState } from "../lib/types";
import "../styles/screens/smart-auto.css";

/** Events the hub serves per page; the hub clamps larger requests to the same limit. */
const PAGE_LIMIT = 1000;
const MAX_PAGES_PER_REFRESH = 10;
/** Newest events kept in the browser; older ones drop off the timeline. */
const MAX_EVENTS = 2000;
/** Rows drawn in the timeline, newest first. */
const MAX_ROWS = 300;
const REFRESH_MS = 3000;
const SHORT_KEY_CHARS = 8;
const ALL = "";
const UNSET = "—";

const STATE_TONE: Readonly<Record<SelectionState, PillTone>> = {
  selected: "info",
  started: "info",
  completed: "ok",
  rejected: "danger",
  cancelled: "warn",
};

const FAILED_STATES: ReadonlySet<SelectionState> = new Set<SelectionState>(["rejected", "cancelled"]);

interface Filters {
  readonly workflow: string;
  readonly worker: string;
  readonly stage: string;
  readonly model: string;
}

const NO_FILTERS: Filters = { workflow: ALL, worker: ALL, stage: ALL, model: ALL };

function shortKey(key: string | null): string {
  return key ? key.slice(0, SHORT_KEY_CHARS) : UNSET;
}

/** The distinct non-empty values `pick` yields, sorted, for one filter's options. */
function distinct(events: readonly SelectionEvent[], pick: (event: SelectionEvent) => string | null): string[] {
  const values = new Set<string>();
  for (const event of events) {
    const value = pick(event);
    if (value) {
      values.add(value);
    }
  }
  return [...values].sort();
}

function matches(event: SelectionEvent, filters: Filters): boolean {
  return (
    (filters.workflow === ALL || event.workflow === filters.workflow) &&
    (filters.worker === ALL || event.worker === filters.worker) &&
    (filters.stage === ALL || event.stage === filters.stage) &&
    (filters.model === ALL || event.model === filters.model)
  );
}

function effortText(event: SelectionEvent): string {
  return event.sent_effort ?? event.selected_effort ?? UNSET;
}

function effortTitle(event: SelectionEvent): string {
  return [
    `requested ${event.requested_effort ?? UNSET}`,
    `selected ${event.selected_effort ?? UNSET}`,
    `sent ${event.sent_effort ?? UNSET}`,
    `confirmed ${event.confirmed_effort ?? UNSET}`,
  ].join(" · ");
}

/** Prompt and completion tokens the upstream reported for one dispatch. */
function tokenText(event: SelectionEvent): string {
  const tokensIn = event.tokens_in === null ? UNSET : formatNumber(event.tokens_in);
  const tokensOut = event.tokens_out === null ? UNSET : formatNumber(event.tokens_out);
  return `${tokensIn} / ${tokensOut}`;
}

function FilterSelect({
  label,
  value,
  options,
  format,
  onChange,
}: {
  readonly label: string;
  readonly value: string;
  readonly options: readonly string[];
  readonly format: (value: string) => string;
  readonly onChange: (value: string) => void;
}) {
  return (
    <label className="smart-auto-filter">
      <span>{label}</span>
      <select className="input" value={value} onChange={(event) => onChange(event.target.value)}>
        <option value={ALL}>All</option>
        {options.map((option) => (
          <option key={option} value={option}>
            {format(option)}
          </option>
        ))}
      </select>
    </label>
  );
}

function EventRow({ event, startsTurn }: { readonly event: SelectionEvent; readonly startsTurn: boolean }) {
  const failed = FAILED_STATES.has(event.state);
  const className = [startsTurn ? "turn-start" : "", failed ? "failed" : ""].filter(Boolean).join(" ");
  return (
    <tr className={className || undefined}>
      <td>{formatDateTime(event.ts_ms)}</td>
      <td className="mono" title={event.turn ?? undefined}>
        {shortKey(event.turn)}
      </td>
      <td className="mono" title={event.worker ?? undefined}>
        {shortKey(event.worker)}
      </td>
      <td className="mono" title={event.workflow ?? undefined}>
        {shortKey(event.workflow)}
      </td>
      <td>{event.stage}</td>
      <td className="mono">{event.model}</td>
      <td>
        <Pill tone={STATE_TONE[event.state]} dot>
          {event.state}
        </Pill>
      </td>
      <td className="smart-auto-reason">{event.reason ?? UNSET}</td>
      <td>{event.role ?? UNSET}</td>
      <td className="mono" title={effortTitle(event)}>
        {effortText(event)}
      </td>
      <td className="num">{event.status ?? UNSET}</td>
      <td className="num">{event.latency_ms === null ? UNSET : formatMs(event.latency_ms)}</td>
      <td className="num">{tokenText(event)}</td>
    </tr>
  );
}

export function SmartAutoScreen() {
  const [events, setEvents] = useState<readonly SelectionEvent[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [filters, setFilters] = useState<Filters>(NO_FILTERS);
  // The last seq the browser holds; each refresh asks only for events after it.
  const cursor = useRef(0);
  const refreshing = useRef(false);

  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      if (refreshing.current) {
        return;
      }
      refreshing.current = true;
      const fresh: SelectionEvent[] = [];
      try {
        for (let page = 0; page < MAX_PAGES_PER_REFRESH; page += 1) {
          const params = new URLSearchParams({ since_seq: String(cursor.current), limit: String(PAGE_LIMIT) });
          const body = await api<SelectionHistoryResponse>(`/api/smart-auto/history?${params}`);
          const last = body.events[body.events.length - 1];
          if (last === undefined) {
            break;
          }
          fresh.push(...body.events);
          cursor.current = last.seq;
          if (body.events.length < PAGE_LIMIT) {
            break;
          }
        }
        if (!cancelled) {
          setUnavailable(false);
          setEvents((current) => [...(current ?? []), ...fresh].slice(-MAX_EVENTS));
        }
      } catch {
        if (!cancelled) {
          setUnavailable(true);
        }
      } finally {
        refreshing.current = false;
      }
    };
    void refresh();
    const timer = setInterval(() => void refresh(), REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  if (unavailable && events === null) {
    return (
      <section className="tab active smart-auto">
        <Panel title="Smart Auto history unavailable">
          <p className="note" role="status">
            Smart Auto history is unavailable. Send a request to <code>hub/smart-auto-balance</code> and reload this tab.
          </p>
        </Panel>
      </section>
    );
  }

  const all = events ?? [];
  const filtered = all.filter((event) => matches(event, filters));
  const rows = filtered.slice(-MAX_ROWS).reverse();
  const failures = filtered.filter((event) => FAILED_STATES.has(event.state)).length;
  const turns = new Set(filtered.flatMap((event) => (event.turn ? [event.turn] : []))).size;
  const tokensIn = filtered.reduce((sum, event) => sum + (event.tokens_in ?? 0), 0);
  const tokensOut = filtered.reduce((sum, event) => sum + (event.tokens_out ?? 0), 0);
  const setFilter = (key: keyof Filters) => (value: string) => setFilters((current) => ({ ...current, [key]: value }));

  return (
    <section className="tab active smart-auto" aria-busy={events === null}>
      <div className="smart-auto-head">
        <span className="dim">Every stage and turn Smart Auto dispatched, newest first. Keys are salted hashes.</span>
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

      <div className="smart-auto-kpis">
        <KpiCard label="Events" value={formatNumber(filtered.length)} sub={`${formatNumber(all.length)} kept in this tab`} />
        <KpiCard
          label="Failures"
          value={formatNumber(failures)}
          sub="Rejected or cancelled dispatches"
          tone={failures > 0 ? "danger" : "default"}
        />
        <KpiCard label="Turns" value={formatNumber(turns)} sub="Distinct turns in view" />
        <KpiCard
          label="Tokens in / out"
          value={`${formatNumber(tokensIn)} / ${formatNumber(tokensOut)}`}
          sub="Reported by upstreams"
        />
      </div>

      <Panel title="Stage and turn timeline" sub={`${formatNumber(rows.length)} shown`} className="smart-auto-panel">
        <div className="smart-auto-toolbar">
          <FilterSelect
            label="Workflow"
            value={filters.workflow}
            options={distinct(all, (event) => event.workflow)}
            format={shortKey}
            onChange={setFilter("workflow")}
          />
          <FilterSelect
            label="Worker"
            value={filters.worker}
            options={distinct(all, (event) => event.worker)}
            format={shortKey}
            onChange={setFilter("worker")}
          />
          <FilterSelect
            label="Stage"
            value={filters.stage}
            options={distinct(all, (event) => event.stage)}
            format={(value) => value}
            onChange={setFilter("stage")}
          />
          <FilterSelect
            label="Model"
            value={filters.model}
            options={distinct(all, (event) => event.model)}
            format={(value) => value}
            onChange={setFilter("model")}
          />
        </div>

        <div className="table-scroll smart-auto-table">
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Turn</th>
                <th>Worker</th>
                <th>Workflow</th>
                <th>Stage</th>
                <th>Model</th>
                <th>State</th>
                <th>Reason</th>
                <th>Role</th>
                <th>Effort</th>
                <th className="num">Status</th>
                <th className="num">Latency</th>
                <th className="num">Tokens in / out</th>
              </tr>
            </thead>
            <tbody>
              {events === null ? (
                <tr>
                  <td colSpan={13} className="table-empty">
                    Loading…
                  </td>
                </tr>
              ) : rows.length === 0 ? (
                <tr>
                  <td colSpan={13} className="table-empty">
                    {all.length === 0
                      ? "No Smart Auto selections yet. Requests to hub/smart-auto-balance appear here as they run."
                      : "No events match these filters."}
                  </td>
                </tr>
              ) : (
                rows.map((event, index) => (
                  <EventRow
                    key={event.seq}
                    event={event}
                    startsTurn={index === 0 || rows[index - 1]?.turn !== event.turn}
                  />
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>
    </section>
  );
}

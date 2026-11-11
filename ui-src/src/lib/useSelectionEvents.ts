import { useEffect, useRef, useState } from "react";

import { api } from "./api";
import type { SelectionEvent, SelectionHistoryResponse } from "./types";

export const REFRESH_MS = 3000;
/** Events kept in the browser. The oldest are dropped first. */
export const MAX_EVENTS = 60_000;
const PAGE_LIMIT = 1000;
const MAX_PAGES_PER_REFRESH = 10;
const MAX_PAGES_PER_LOAD = MAX_EVENTS / PAGE_LIMIT;
const HOUR_MS = 3_600_000;
const DAY_MS = 24 * HOUR_MS;

export type SelectionRange = "1d" | "7d" | "30d" | "all";

export const SELECTION_RANGE_MS: Readonly<Record<SelectionRange, number | null>> = {
  "1d": DAY_MS,
  "7d": 7 * DAY_MS,
  "30d": 30 * DAY_MS,
  all: null,
};

export interface SelectionFeed {
  /** `null` until the first answer arrives. */
  readonly events: readonly SelectionEvent[] | null;
  /** True when the last refresh failed. */
  readonly unavailable: boolean;
}

function historyUrl(cursor: number, sinceTsMs: number | null): string {
  const params = new URLSearchParams({ since_seq: String(cursor), limit: String(PAGE_LIMIT) });
  if (sinceTsMs !== null) {
    params.set("since_ts_ms", String(sinceTsMs));
  }
  return `/api/smart-auto/history?${params}`;
}

/**
 * Loads the events of `range` from the hub's saved history, then polls `GET /api/smart-auto/history` every
 * {@link REFRESH_MS} for the newest events. A refresh asks only for events after the last seq it holds.
 */
export function useSelectionEvents(range: SelectionRange): SelectionFeed {
  const [events, setEvents] = useState<readonly SelectionEvent[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const cursor = useRef(0);
  const refreshing = useRef(false);
  const loaded = useRef(false);

  useEffect(() => {
    let cancelled = false;
    cursor.current = 0;
    loaded.current = false;
    setEvents(null);
    const windowMs = SELECTION_RANGE_MS[range];
    const sinceTsMs = windowMs === null ? 0 : Date.now() - windowMs;

    const refresh = async () => {
      if (refreshing.current) {
        return;
      }
      refreshing.current = true;
      const initial = !loaded.current;
      const fresh: SelectionEvent[] = [];
      try {
        const maxPages = initial ? MAX_PAGES_PER_LOAD : MAX_PAGES_PER_REFRESH;
        for (let page = 0; page < maxPages; page += 1) {
          const body = await api<SelectionHistoryResponse>(historyUrl(cursor.current, initial ? sinceTsMs : null));
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
          loaded.current = true;
          setUnavailable(false);
          setEvents((current) => [...(initial ? [] : (current ?? [])), ...fresh].slice(-MAX_EVENTS));
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
  }, [range]);

  return { events, unavailable };
}

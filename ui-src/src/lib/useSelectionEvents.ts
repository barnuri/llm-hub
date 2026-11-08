import { useEffect, useRef, useState } from "react";

import { api } from "./api";
import type { SelectionEvent, SelectionHistoryResponse } from "./types";

export const REFRESH_MS = 3000;
/** Events kept in the browser. The oldest are dropped first. */
export const MAX_EVENTS = 2000;
const PAGE_LIMIT = 1000;
const MAX_PAGES_PER_REFRESH = 10;

export interface SelectionFeed {
  /** `null` until the first answer arrives. */
  readonly events: readonly SelectionEvent[] | null;
  /** True when the last refresh failed. */
  readonly unavailable: boolean;
}

/**
 * Polls `GET /api/smart-auto/history` every {@link REFRESH_MS}. Each refresh asks only for events after the last seq it holds.
 */
export function useSelectionEvents(): SelectionFeed {
  const [events, setEvents] = useState<readonly SelectionEvent[] | null>(null);
  const [unavailable, setUnavailable] = useState(false);
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

  return { events, unavailable };
}

import { useEffect, useState } from "react";

import { savedKey } from "../lib/api";
import { latencyHeat, ttftHeat } from "../lib/colors";
import { formatMs, formatNumber, formatTime } from "../lib/format";
import { useLive } from "../lib/live";
import type { UsageRow } from "../lib/types";
import { shortModel } from "./UsageModelChart";
import { Heat } from "./Heat";
import { ModelName } from "./ModelName";
import { ModelTag } from "./ModelTag";
import { Panel } from "./Panel";
import { Pill } from "./Pill";
import "./LiveFeed.css";

const MAX_EVENTS = 30;
const RECONNECT_MIN_MS = 2_000;
const RECONNECT_MAX_MS = 15_000;
const ERROR_STATUS = 400;
const EVENT_SEPARATOR = "\n\n";
const DATA_PREFIX = "data:";

type FeedState = "connecting" | "live" | "offline" | "unsupported";

interface FeedEvent {
  readonly id: number;
  readonly row: UsageRow;
}

const STATE_TEXT: Readonly<Record<FeedState, string>> = {
  connecting: "connecting",
  live: "streaming",
  offline: "reconnecting",
  unsupported: "not available",
};

function parseBlock(block: string): UsageRow | null {
  const data = block
    .split("\n")
    .filter((line) => line.startsWith(DATA_PREFIX))
    .map((line) => line.slice(DATA_PREFIX.length).trim())
    .join("");
  if (data === "") {
    return null;
  }
  try {
    return JSON.parse(data) as UsageRow;
  } catch {
    return null;
  }
}

export function LiveFeed() {
  const { paused } = useLive();
  const [events, setEvents] = useState<readonly FeedEvent[]>([]);
  const [state, setState] = useState<FeedState>("connecting");

  useEffect(() => {
    if (paused) {
      return;
    }
    const abort = new AbortController();
    let nextId = 0;
    let delay = RECONNECT_MIN_MS;

    const run = async () => {
      while (!abort.signal.aborted) {
        try {
          const headers = new Headers({ accept: "text/event-stream" });
          const key = savedKey();
          if (key) {
            headers.set("authorization", `Bearer ${key}`);
          }
          const response = await fetch("/api/live", { headers, signal: abort.signal });
          if (response.status === 404 || response.status === 405) {
            setState("unsupported");
            return;
          }
          if (!response.ok || !response.body) {
            throw new Error(`live feed answered ${response.status}`);
          }
          setState("live");
          delay = RECONNECT_MIN_MS;
          const reader = response.body.getReader();
          const decoder = new TextDecoder();
          let buffer = "";
          for (;;) {
            const { done, value } = await reader.read();
            if (done) {
              break;
            }
            buffer += decoder.decode(value, { stream: true });
            const blocks = buffer.split(EVENT_SEPARATOR);
            buffer = blocks.pop() ?? "";
            const rows = blocks.map(parseBlock).filter((row): row is UsageRow => row !== null);
            if (rows.length > 0) {
              const added = rows.map((row) => ({ id: (nextId += 1), row }));
              setEvents((current) => [...added.reverse(), ...current].slice(0, MAX_EVENTS));
            }
          }
        } catch {
          if (abort.signal.aborted) {
            return;
          }
        }
        setState("offline");
        await new Promise((resolve) => setTimeout(resolve, delay));
        delay = Math.min(delay * 2, RECONNECT_MAX_MS);
      }
    };

    void run();
    return () => abort.abort();
  }, [paused]);

  const status = paused ? "paused" : STATE_TEXT[state];

  return (
    <Panel title="Live requests" sub={status}>
      {state === "unsupported" ? (
        <p className="note" role="status">
          This hub build has no live feed. Update and restart the hub to turn it on.
        </p>
      ) : events.length === 0 ? (
        <p className="note" role="status">
          No requests yet. Each request appears here when it finishes.
        </p>
      ) : (
        <ul className="live-feed" aria-live="off">
          {events.map(({ id, row }) => (
            <li key={id} className="live-feed-row">
              <span className="live-feed-time mono">{formatTime(row.ts_ms)}</span>
              <span className="live-feed-model mono">
                <ModelTag colorKey={shortModel(row.model)}>
                  <ModelName id={row.model} profile={row.profile} />
                </ModelTag>
              </span>
              <Pill tone={row.status >= ERROR_STATUS ? "danger" : "ok"} title={row.error ?? undefined}>
                {row.status}
              </Pill>
              <span className="live-feed-meta mono">
                <Heat level={ttftHeat(row.ttft_ms ?? 0)}>{formatMs(row.ttft_ms ?? 0)}</Heat>
                <Heat level={latencyHeat(row.latency_ms)}>{formatMs(row.latency_ms)}</Heat>
                <span className="live-feed-tokens">
                  {formatNumber(row.tokens_in)} / {formatNumber(row.tokens_out)} tok
                </span>
              </span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

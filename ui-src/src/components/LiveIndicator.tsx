import { useEffect, useState } from "react";

import { toggleLivePaused, useLive } from "../lib/live";
import "./LiveIndicator.css";

const AGE_TICK_MS = 5_000;
const SECOND_MS = 1_000;
const MINUTE_S = 60;

interface LiveIndicatorProps {
  /** Epoch ms of the last successful load, or null before the first one. */
  readonly updatedAt: number | null;
}

function ageLabel(updatedAt: number | null, now: number): string {
  if (updatedAt === null) {
    return "loading";
  }
  const seconds = Math.max(0, Math.round((now - updatedAt) / SECOND_MS));
  if (seconds < 10) {
    return "just now";
  }
  if (seconds < MINUTE_S) {
    return `${seconds}s ago`;
  }
  return `${Math.floor(seconds / MINUTE_S)}m ago`;
}

export function LiveIndicator({ updatedAt }: LiveIndicatorProps) {
  const { paused } = useLive();
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), AGE_TICK_MS);
    return () => clearInterval(timer);
  }, []);

  return (
    <button
      type="button"
      className={paused ? "live-indicator paused" : "live-indicator"}
      aria-pressed={paused}
      title={paused ? "Updates are paused. Click to resume." : "Updates every 15 s. Click to pause."}
      onClick={toggleLivePaused}
    >
      <span className="live-dot" aria-hidden="true" />
      <span className="live-word" role="status">
        {paused ? "Paused" : "Live"}
      </span>
      <span className="live-age">{ageLabel(updatedAt, now)}</span>
    </button>
  );
}

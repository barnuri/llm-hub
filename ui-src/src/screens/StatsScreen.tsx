import { useEffect, useState } from "react";

import { StatsTable } from "../components/StatsTable";
import { api } from "../lib/api";
import type { StatsSnapshot } from "../lib/types";

interface StatsScreenProps {
  readonly onError: (message: string) => void;
}

const REFRESH_MS = 5000;

export function StatsScreen({ onError }: StatsScreenProps) {
  const [snapshot, setSnapshot] = useState<StatsSnapshot | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = () => {
      api<StatsSnapshot>("/api/stats")
        .then((body) => {
          if (!cancelled) {
            setSnapshot(body);
          }
        })
        .catch((err) => onError(err instanceof Error ? err.message : String(err)));
    };
    load();
    const timer = setInterval(load, REFRESH_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [onError]);

  return (
    <section className="tab active">
      <p className="note">
        Live counters since process start. Enable <code>LLM_HUB_PERSISTENT=true</code> for durable history.
      </p>
      <StatsTable label="Profile" rows={snapshot?.profiles ?? []} />
      <StatsTable label="Model" rows={snapshot?.models ?? []} />
    </section>
  );
}

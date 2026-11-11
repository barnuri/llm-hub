import { useEffect, useMemo, useState } from "react";

import { api } from "../lib/api";
import { formatNumber, formatUsd } from "../lib/format";
import { useLive } from "../lib/live";
import { useElementWidth } from "../lib/useElementWidth";
import { formatBucket, type SeriesBucket } from "../lib/series";
import type { SeriesPoint, StatsRange, StatsSnapshot } from "../lib/types";
import { Panel } from "./Panel";
import { Segmented, type SegmentedOption } from "./Segmented";
import "./CostOverTimePanel.css";

type CostRange = Extract<StatsRange, "1d" | "7d" | "30d" | "all">;

const RANGE_OPTIONS: readonly SegmentedOption<CostRange>[] = [
  { value: "1d", label: "24 h" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "all", label: "All time" },
];
const SMART_AUTO_ROUTE = "smart-auto";
const RANGE_TEXT: Readonly<Record<CostRange, string>> = {
  "1d": "last 24 hours",
  "7d": "last 7 days",
  "30d": "last 30 days",
  all: "all stored history",
};
const VIEW_HEIGHT = 200;
const PAD = { top: 10, right: 8, bottom: 24, left: 46 } as const;
const INNER_HEIGHT = VIEW_HEIGHT - PAD.top - PAD.bottom;
const Y_TICKS = 3;
const X_TICKS = 6;
const BAR_GAP = 0.25;

interface CostBucket {
  readonly ts: number;
  readonly cost: number;
  readonly requests: number;
}

function bucketsOf(series: readonly SeriesPoint[]): CostBucket[] {
  const byTime = new Map<number, { cost: number; requests: number }>();
  for (const point of series) {
    const entry = byTime.get(point.ts_ms) ?? { cost: 0, requests: 0 };
    entry.cost += point.cost_usd;
    entry.requests += point.requests;
    byTime.set(point.ts_ms, entry);
  }
  return [...byTime.entries()].sort(([left], [right]) => left - right).map(([ts, entry]) => ({ ts, ...entry }));
}

function usdTick(value: number): string {
  return value >= 100 ? `$${Math.round(value)}` : formatUsd(value);
}

export function CostOverTimePanel() {
  const { tick } = useLive();
  const [range, setRange] = useState<CostRange>("7d");
  const [snapshot, setSnapshot] = useState<StatsSnapshot | null>(null);
  const [hubTotal, setHubTotal] = useState<number | null>(null);
  const [failed, setFailed] = useState(false);
  const [hovered, setHovered] = useState<number | null>(null);
  const [frame, viewWidth] = useElementWidth<HTMLDivElement>();
  const innerWidth = viewWidth - PAD.left - PAD.right;

  useEffect(() => {
    let cancelled = false;
    Promise.all([
      api<StatsSnapshot>(`/api/stats?range=${range}&routed_by=${SMART_AUTO_ROUTE}`),
      api<StatsSnapshot>(`/api/stats?range=${range}`),
    ])
      .then(([smartAuto, everything]) => {
        if (!cancelled) {
          setSnapshot(smartAuto);
          setHubTotal(everything.overview.cost_usd);
          setFailed(false);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setFailed(true);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [range, tick]);

  const buckets = useMemo(() => bucketsOf(snapshot?.series ?? []), [snapshot]);
  const bucket: SeriesBucket = snapshot?.series_bucket === "hour" ? "hour" : "day";
  const total = buckets.reduce((sum, entry) => sum + entry.cost, 0);
  const peak = Math.max(0, ...buckets.map((entry) => entry.cost));
  const slot = buckets.length > 0 ? innerWidth / buckets.length : innerWidth;
  const hoveredBucket = hovered === null ? null : (buckets[hovered] ?? null);
  const shareText =
    hubTotal !== null && hubTotal > 0
      ? ` · ${formatUsd(hubTotal)} total hub cost (${Math.round((total / hubTotal) * 100)}%)`
      : "";
  const tickEvery = Math.max(1, Math.ceil(buckets.length / X_TICKS));

  const body = () => {
    if (failed) {
      return (
        <p className="note" role="status">
          Cost history is unavailable. Turn on persistence with <code>LLM_HUB_PERSISTENT=true</code> to keep it.
        </p>
      );
    }
    if (snapshot === null) {
      return <div className="skeleton cost-over-time-skeleton" />;
    }
    if (buckets.length === 0) {
      return <p className="note">No Smart Auto requests in this window.</p>;
    }
    return (
      <>
        <p className="cost-over-time-readout mono" aria-live="off">
          {hoveredBucket
            ? `${formatBucket(hoveredBucket.ts, bucket)} · ${formatUsd(hoveredBucket.cost)} · ${formatNumber(hoveredBucket.requests)} requests`
            : `${formatUsd(total)} by Smart Auto in the ${RANGE_TEXT[range]}${shareText} · peak ${formatUsd(peak)} per ${bucket}`}
        </p>
        <svg
          className="cost-over-time-chart"
          width={viewWidth}
          height={VIEW_HEIGHT}
          viewBox={`0 0 ${viewWidth} ${VIEW_HEIGHT}`}
          role="img"
          aria-label={`Smart Auto cost per ${bucket}, ${RANGE_TEXT[range]}, ${formatUsd(total)} in total`}
          onPointerLeave={() => setHovered(null)}
        >
          {Array.from({ length: Y_TICKS + 1 }, (_, index) => {
            const value = (peak * index) / Y_TICKS;
            const y = PAD.top + INNER_HEIGHT * (1 - index / Y_TICKS);
            return (
              <g key={index}>
                <line className="cost-over-time-grid" x1={PAD.left} x2={viewWidth - PAD.right} y1={y} y2={y} />
                <text className="cost-over-time-tick" x={PAD.left - 6} y={y + 3} textAnchor="end">
                  {usdTick(value)}
                </text>
              </g>
            );
          })}
          {buckets.map((entry, index) => {
            const height = peak > 0 ? (entry.cost / peak) * INNER_HEIGHT : 0;
            const x = PAD.left + index * slot;
            return (
              <g key={entry.ts}>
                <rect
                  className={index === hovered ? "cost-over-time-bar hot" : "cost-over-time-bar"}
                  x={x + (slot * BAR_GAP) / 2}
                  y={PAD.top + INNER_HEIGHT - height}
                  width={Math.max(1, slot * (1 - BAR_GAP))}
                  height={Math.max(height, entry.cost > 0 ? 1 : 0)}
                  rx={2}
                />
                <rect
                  className="cost-over-time-hit"
                  x={x}
                  y={PAD.top}
                  width={slot}
                  height={INNER_HEIGHT}
                  onPointerEnter={() => setHovered(index)}
                />
                {index % tickEvery === 0 ? (
                  <text className="cost-over-time-tick" x={x + slot / 2} y={VIEW_HEIGHT - 6} textAnchor="middle">
                    {formatBucket(entry.ts, bucket)}
                  </text>
                ) : null}
              </g>
            );
          })}
        </svg>
      </>
    );
  };

  return (
    <Panel
      className="cost-over-time"
      title="Smart Auto cost over time"
      sub="Requests routed by Smart Auto"
      actions={<Segmented label="Cost range" options={RANGE_OPTIONS} value={range} onChange={setRange} />}
    >
      <div ref={frame} className="cost-over-time-frame">
        {body()}
      </div>
    </Panel>
  );
}

import { useMemo, useState } from "react";

import { formatNumber, formatUsd } from "../lib/format";
import type { SeriesPoint, StatsEntry } from "../lib/types";

export const DEFAULT_VISIBLE_MODELS = 4;
const VIEW_WIDTH = 640;
const VIEW_HEIGHT = 168;
const PAD = { top: 12, right: 46, bottom: 26, left: 38 } as const;
const INNER_WIDTH = VIEW_WIDTH - PAD.left - PAD.right;
const INNER_HEIGHT = VIEW_HEIGHT - PAD.top - PAD.bottom;
const TICK_COUNT = 3;
const MODEL_COLORS: readonly string[] = [
  "#38bdf8",
  "#22c55e",
  "#f59e0b",
  "#a78bfa",
  "#f472b6",
  "#2dd4bf",
  "#fb7185",
  "#eab308",
];

export type SeriesBucket = "hour" | "day";

export function shortModel(key: string): string {
  const slash = key.lastIndexOf("/");
  if (slash < 0 || slash === key.length - 1) {
    return key;
  }
  return key.slice(slash + 1);
}

export function modelColor(key: string, ranked: readonly string[]): string {
  const index = ranked.indexOf(key);
  const safeIndex = index >= 0 ? index : 0;
  return MODEL_COLORS[safeIndex % MODEL_COLORS.length] ?? MODEL_COLORS[0];
}

export function rankModels(models: readonly StatsEntry[]): string[] {
  return [...models]
    .filter((entry) => entry.requests > 0 || entry.cost_usd > 0)
    .sort((left, right) => {
      if (right.requests !== left.requests) {
        return right.requests - left.requests;
      }
      return right.cost_usd - left.cost_usd;
    })
    .map((entry) => entry.key);
}

export function defaultHidden(ranked: readonly string[]): Set<string> {
  return new Set(ranked.slice(DEFAULT_VISIBLE_MODELS));
}

function formatBucket(tsMs: number, bucket: SeriesBucket): string {
  const date = new Date(tsMs);
  if (bucket === "hour") {
    return date.toLocaleTimeString(undefined, { hour: "numeric" });
  }
  return date.toLocaleDateString(undefined, { weekday: "short", day: "numeric" });
}

function uniqueTimes(series: readonly SeriesPoint[]): number[] {
  return [...new Set(series.map((point) => point.ts_ms))].sort((left, right) => left - right);
}

interface UsageChartLegendProps {
  readonly ranked: readonly string[];
  readonly hidden: ReadonlySet<string>;
  readonly onToggle: (key: string) => void;
  readonly showRequests: boolean;
  readonly showCost: boolean;
  readonly onToggleRequests: () => void;
  readonly onToggleCost: () => void;
}

export function UsageChartLegend({
  ranked,
  hidden,
  onToggle,
  showRequests,
  showCost,
  onToggleRequests,
  onToggleCost,
}: UsageChartLegendProps) {
  if (ranked.length === 0) {
    return null;
  }
  return (
    <div className="usage-chart-controls">
      <div className="usage-chart-legend" role="group" aria-label="Toggle metrics">
        <button
          type="button"
          className={showRequests ? "usage-chart-swatch requests active" : "usage-chart-swatch requests"}
          aria-pressed={showRequests}
          onClick={onToggleRequests}
        >
          Requests
        </button>
        <button
          type="button"
          className={showCost ? "usage-chart-swatch cost active" : "usage-chart-swatch cost"}
          aria-pressed={showCost}
          onClick={onToggleCost}
        >
          Cost
        </button>
      </div>
      <div className="usage-model-chips" role="group" aria-label="Toggle models">
        {ranked.map((key) => {
          const active = !hidden.has(key);
          return (
            <button
              key={key}
              type="button"
              className={active ? "usage-model-chip active" : "usage-model-chip"}
              aria-pressed={active}
              title={key}
              onClick={() => onToggle(key)}
            >
              <span className="usage-model-chip-swatch" style={{ background: modelColor(key, ranked) }} />
              {shortModel(key)}
            </button>
          );
        })}
      </div>
    </div>
  );
}

interface UsageTrendChartProps {
  readonly title: string;
  readonly hint: string;
  readonly bucket: SeriesBucket;
  readonly series: readonly SeriesPoint[];
  readonly ranked: readonly string[];
  readonly hidden: ReadonlySet<string>;
  readonly showRequests: boolean;
  readonly showCost: boolean;
  readonly onToggle: (key: string) => void;
}

export function UsageTrendChart({
  title,
  hint,
  bucket,
  series,
  ranked,
  hidden,
  showRequests,
  showCost,
  onToggle,
}: UsageTrendChartProps) {
  const [hoverTs, setHoverTs] = useState<number | null>(null);
  const times = useMemo(() => uniqueTimes(series), [series]);
  const visible = ranked.filter((key) => !hidden.has(key));
  const requestsOn = showRequests || !showCost;
  const costOn = showCost || !showRequests;

  const maxRequests = Math.max(
    1,
    ...series.filter((point) => visible.includes(point.key)).map((point) => point.requests),
  );
  const maxCost = Math.max(
    0,
    ...series.filter((point) => visible.includes(point.key)).map((point) => point.cost_usd),
  );

  if (times.length === 0 || visible.length === 0) {
    return (
      <section className="usage-trend">
        <h2>{title}</h2>
        <p className="dim">{hint}</p>
        <p className="note">No model traffic in this window.</p>
      </section>
    );
  }

  const xFor = (ts: number): number => {
    if (times.length === 1) {
      return PAD.left + INNER_WIDTH / 2;
    }
    const index = times.indexOf(ts);
    const safeIndex = index >= 0 ? index : 0;
    return PAD.left + (safeIndex / (times.length - 1)) * INNER_WIDTH;
  };
  const requestY = (value: number): number => PAD.top + INNER_HEIGHT * (1 - value / maxRequests);
  const costY = (value: number): number =>
    PAD.top + INNER_HEIGHT * (1 - (maxCost > 0 ? value / maxCost : 0));

  const tickStep = bucket === "hour" ? 4 : 1;
  const hover = hoverTs === null ? null : series.filter((point) => point.ts_ms === hoverTs);

  const pathFor = (key: string, metric: "requests" | "cost"): string => {
    const points = times.map((ts) => {
      const row = series.find((point) => point.ts_ms === ts && point.key === key);
      const value = metric === "requests" ? (row?.requests ?? 0) : (row?.cost_usd ?? 0);
      const y = metric === "requests" ? requestY(value) : costY(value);
      return `${xFor(ts).toFixed(1)},${y.toFixed(1)}`;
    });
    return `M${points.join(" L")}`;
  };

  return (
    <section className="usage-trend">
      <h2>{title}</h2>
      <p className="dim">{hint}</p>
      <div className="usage-chart usage-chart-compact">
        <svg
          className="usage-chart-svg"
          viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`}
          role="img"
          aria-label={`${title}: ${requestsOn ? "requests" : ""} ${costOn ? "cost" : ""} by model`}
          onMouseLeave={() => setHoverTs(null)}
          onMouseMove={(event) => {
            const svg = event.currentTarget;
            const box = svg.getBoundingClientRect();
            const x = ((event.clientX - box.left) / box.width) * VIEW_WIDTH;
            let nearest = times[0] ?? 0;
            let best = Number.POSITIVE_INFINITY;
            for (const ts of times) {
              const distance = Math.abs(xFor(ts) - x);
              if (distance < best) {
                best = distance;
                nearest = ts;
              }
            }
            setHoverTs(nearest);
          }}
        >
          {Array.from({ length: TICK_COUNT + 1 }, (_, index) => {
            const ratio = index / TICK_COUNT;
            const y = PAD.top + INNER_HEIGHT * (1 - ratio);
            return (
              <g key={index}>
                <line className="usage-chart-grid" x1={PAD.left} x2={VIEW_WIDTH - PAD.right} y1={y} y2={y} />
                {requestsOn ? (
                  <text className="usage-chart-tick left" x={PAD.left - 5} y={y + 3} textAnchor="end">
                    {formatNumber(Math.round(maxRequests * ratio))}
                  </text>
                ) : null}
                {costOn ? (
                  <text className="usage-chart-tick right" x={VIEW_WIDTH - PAD.right + 5} y={y + 3}>
                    {formatUsd(maxCost * ratio)}
                  </text>
                ) : null}
              </g>
            );
          })}

          {hoverTs !== null ? (
            <line
              className="usage-chart-hover"
              x1={xFor(hoverTs)}
              x2={xFor(hoverTs)}
              y1={PAD.top}
              y2={PAD.top + INNER_HEIGHT}
            />
          ) : null}

          {visible.map((key) => (
            <g key={key} className="usage-chart-series" onClick={() => onToggle(key)}>
              {requestsOn ? (
                <>
                  <path d={pathFor(key, "requests")} className="usage-chart-hit-line" />
                  <path d={pathFor(key, "requests")} className="usage-chart-line requests" stroke={modelColor(key, ranked)} />
                </>
              ) : null}
              {costOn ? (
                <>
                  <path d={pathFor(key, "cost")} className="usage-chart-hit-line" />
                  <path
                    d={pathFor(key, "cost")}
                    className="usage-chart-line cost"
                    stroke={modelColor(key, ranked)}
                  />
                </>
              ) : null}
            </g>
          ))}

          {times.map((ts, index) =>
            index % tickStep === 0 || index === times.length - 1 ? (
              <text key={ts} className="usage-chart-xlabel" x={xFor(ts)} y={VIEW_HEIGHT - 8} textAnchor="middle">
                {formatBucket(ts, bucket)}
              </text>
            ) : null,
          )}
        </svg>
        <div className="usage-chart-tooltip" role="status">
          {hover && hoverTs !== null ? (
            <>
              <div className="mono">{formatBucket(hoverTs, bucket)}</div>
              {visible.map((key) => {
                const row = hover.find((point) => point.key === key);
                return (
                  <div key={key}>
                    <span style={{ color: modelColor(key, ranked) }}>{shortModel(key)}</span>
                    {": "}
                    {requestsOn ? `${formatNumber(row?.requests ?? 0)} req` : ""}
                    {requestsOn && costOn ? " · " : ""}
                    {costOn ? formatUsd(row?.cost_usd ?? 0) : ""}
                  </div>
                );
              })}
            </>
          ) : (
            <span className="dim">Hover a time, click a model chip or line to show or hide it.</span>
          )}
        </div>
      </div>
      <table className="sr-only">
        <caption>{title} model requests and cost</caption>
        <thead>
          <tr>
            <th>Time</th>
            <th>Model</th>
            <th>Requests</th>
            <th>Cost</th>
          </tr>
        </thead>
        <tbody>
          {series.map((point) => (
            <tr key={`${point.ts_ms}-${point.key}`}>
              <td>{formatBucket(point.ts_ms, bucket)}</td>
              <td>{point.key}</td>
              <td>{formatNumber(point.requests)}</td>
              <td>{formatUsd(point.cost_usd)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

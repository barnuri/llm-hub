import { formatNumber } from "../lib/format";
import { formatBucket, type SeriesBucket, type UpstreamStacks } from "../lib/series";

const VIEW_WIDTH = 640;
const VIEW_HEIGHT = 220;
const PAD = { top: 10, right: 6, bottom: 24, left: 40 } as const;
const INNER_WIDTH = VIEW_WIDTH - PAD.left - PAD.right;
const INNER_HEIGHT = VIEW_HEIGHT - PAD.top - PAD.bottom;
const TICK_COUNT = 4;
const BAR_FILL_RATIO = 0.62;
const HOUR_LABEL_STEP = 4;
const NICE_STEPS: readonly number[] = [1, 2, 5, 10];

function niceStep(rawStep: number): number {
  if (rawStep <= 1) {
    return 1;
  }
  const magnitude = 10 ** Math.floor(Math.log10(rawStep));
  const factor = NICE_STEPS.find((step) => step * magnitude >= rawStep) ?? 10;
  return factor * magnitude;
}

interface UpstreamBarsChartProps {
  readonly stacks: UpstreamStacks;
  readonly bucket: SeriesBucket;
  readonly hovered: number | null;
  readonly onHover: (index: number | null) => void;
}

export function UpstreamBarsChart({ stacks, bucket, hovered, onHover }: UpstreamBarsChartProps) {
  const { groups, buckets } = stacks;
  const peak = Math.max(0, ...buckets.map((entry) => entry.total));
  if (buckets.length === 0 || peak === 0) {
    return <p className="note">No traffic in this window.</p>;
  }

  const step = niceStep(peak / TICK_COUNT);
  const axisMax = step * TICK_COUNT;
  const slot = INNER_WIDTH / buckets.length;
  const barWidth = slot * BAR_FILL_RATIO;
  const yFor = (value: number): number => PAD.top + INNER_HEIGHT * (1 - value / axisMax);
  const labelEvery = bucket === "hour" ? HOUR_LABEL_STEP : 1;

  return (
    <div className="upstream-chart">
      <svg
        className="upstream-chart-svg"
        viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`}
        role="img"
        aria-label={`Requests by upstream, per ${bucket}`}
        onMouseLeave={() => onHover(null)}
      >
        {Array.from({ length: TICK_COUNT + 1 }, (_, index) => {
          const value = step * index;
          const y = yFor(value);
          return (
            <g key={value}>
              <line className="upstream-chart-grid" x1={PAD.left} x2={VIEW_WIDTH - PAD.right} y1={y} y2={y} />
              <text className="upstream-chart-tick" x={PAD.left - 6} y={y + 3} textAnchor="end">
                {formatNumber(value)}
              </text>
            </g>
          );
        })}

        {buckets.map((entry, index) => {
          const x = PAD.left + slot * index + (slot - barWidth) / 2;
          let base = 0;
          const className =
            hovered === null || hovered === index ? "upstream-chart-bar" : "upstream-chart-bar dimmed";
          return (
            <g key={entry.ts} className={className} onMouseEnter={() => onHover(index)}>
              <rect
                className="upstream-chart-hit"
                x={PAD.left + slot * index}
                y={PAD.top}
                width={slot}
                height={INNER_HEIGHT}
              />
              {groups.map((group, groupIndex) => {
                const value = entry.values[groupIndex] ?? 0;
                if (value === 0) {
                  return null;
                }
                const top = yFor(base + value);
                const height = yFor(base) - top;
                base += value;
                return (
                  <rect
                    key={group.key}
                    className={`upstream-chart-segment tone-${group.tone}`}
                    x={x}
                    y={top}
                    width={barWidth}
                    height={height}
                  />
                );
              })}
            </g>
          );
        })}

        {buckets.map((entry, index) =>
          index % labelEvery === 0 || index === buckets.length - 1 ? (
            <text
              key={entry.ts}
              className="upstream-chart-xlabel"
              x={PAD.left + slot * index + slot / 2}
              y={VIEW_HEIGHT - 6}
              textAnchor="middle"
            >
              {formatBucket(entry.ts, bucket)}
            </text>
          ) : null,
        )}
      </svg>

      <ul className="upstream-legend" aria-label="Upstreams">
        {groups.map((group) => (
          <li key={group.key} className="upstream-legend-item">
            <span className={`upstream-legend-swatch tone-${group.tone}`} aria-hidden="true" />
            <span className="mono">{group.label}</span>
            <span className="dim">{formatNumber(group.total)}</span>
          </li>
        ))}
      </ul>

      <table className="sr-only">
        <caption>Requests by upstream</caption>
        <thead>
          <tr>
            <th>Time</th>
            {groups.map((group) => (
              <th key={group.key}>{group.label}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {buckets.map((entry) => (
            <tr key={entry.ts}>
              <td>{formatBucket(entry.ts, bucket)}</td>
              {groups.map((group, groupIndex) => (
                <td key={group.key}>{formatNumber(entry.values[groupIndex] ?? 0)}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

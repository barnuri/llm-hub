import { formatDateTime, formatNumber } from "../lib/format";
import type { SessionCompaction, SessionPromptPoint } from "../lib/sessions";

const VIEW_WIDTH = 640;
const VIEW_HEIGHT = 220;
const PAD = { top: 12, right: 44, bottom: 26, left: 48 } as const;
const INNER_WIDTH = VIEW_WIDTH - PAD.left - PAD.right;
const INNER_HEIGHT = VIEW_HEIGHT - PAD.top - PAD.bottom;
const REFERENCE_TOKENS = 200_000;
const TICK_STEPS: readonly number[] = [25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000];
const MAX_TICKS = 5;
const HEADROOM = 1.08;
const COMPACTION_RADIUS = 4;

interface PromptSizeChartProps {
  readonly series: readonly SessionPromptPoint[];
  readonly compactions: readonly SessionCompaction[];
}

function tickStep(maxValue: number): number {
  return TICK_STEPS.find((step) => maxValue / step <= MAX_TICKS) ?? TICK_STEPS[TICK_STEPS.length - 1] ?? 1_000_000;
}

function tokenLabel(tokens: number): string {
  if (tokens >= 1_000_000) {
    return `${formatNumber(tokens / 1_000_000, 1)}M`;
  }
  return `${formatNumber(Math.round(tokens / 1000))}k`;
}

export function PromptSizeChart({ series, compactions }: PromptSizeChartProps) {
  if (series.length === 0) {
    return <p className="note">No requests with a prompt size in this window.</p>;
  }
  const timestamps = series.map((point) => point.ts_ms);
  const minTs = Math.min(...timestamps);
  const maxTs = Math.max(...timestamps);
  const tsSpan = maxTs - minTs;
  const peakTokens = Math.max(...series.map((point) => point.prompt_tokens));
  const yMax = Math.max(REFERENCE_TOKENS, peakTokens) * HEADROOM;
  const step = tickStep(yMax);
  const ticks: number[] = [];
  for (let value = 0; value <= yMax; value += step) {
    ticks.push(value);
  }

  const xOf = (tsMs: number): number =>
    PAD.left + (tsSpan > 0 ? ((tsMs - minTs) / tsSpan) * INNER_WIDTH : INNER_WIDTH / 2);
  const yOf = (tokens: number): number => PAD.top + INNER_HEIGHT - (tokens / yMax) * INNER_HEIGHT;
  const linePoints = series
    .map((point) => `${xOf(point.ts_ms).toFixed(1)},${yOf(point.prompt_tokens).toFixed(1)}`)
    .join(" ");
  const referenceY = yOf(REFERENCE_TOKENS);
  const baseline = PAD.top + INNER_HEIGHT;

  return (
    <svg
      className="prompt-chart"
      viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`}
      role="img"
      aria-label={`Prompt size per request: ${formatNumber(series.length)} points, peak ${formatNumber(peakTokens)} tokens, ${formatNumber(compactions.length)} compactions`}
    >
      {ticks.map((value) => (
        <g key={value}>
          <line className="prompt-chart-grid" x1={PAD.left} x2={PAD.left + INNER_WIDTH} y1={yOf(value)} y2={yOf(value)} />
          <text className="prompt-chart-tick" x={PAD.left - 6} y={yOf(value) + 3} textAnchor="end">
            {tokenLabel(value)}
          </text>
        </g>
      ))}
      <line
        className="prompt-chart-reference"
        x1={PAD.left}
        x2={PAD.left + INNER_WIDTH}
        y1={referenceY}
        y2={referenceY}
      />
      <text className="prompt-chart-tick prompt-chart-reference-label" x={PAD.left + INNER_WIDTH + 6} y={referenceY + 3}>
        200k
      </text>
      <polyline className="prompt-chart-line" points={linePoints} />
      {compactions.map((compaction) => (
        <circle
          key={`${compaction.ts_ms}-${compaction.prompt_tokens}`}
          className="prompt-chart-compaction"
          cx={xOf(compaction.ts_ms)}
          cy={yOf(compaction.prompt_tokens)}
          r={COMPACTION_RADIUS}
        >
          <title>
            {`Compaction ${formatDateTime(compaction.ts_ms)}: ${formatNumber(compaction.previous_prompt_tokens)} to ${formatNumber(compaction.prompt_tokens)} tokens (-${formatNumber(compaction.drop_percent)}%)`}
          </title>
        </circle>
      ))}
      <text className="prompt-chart-tick" x={PAD.left} y={baseline + 16}>
        {formatDateTime(minTs)}
      </text>
      <text className="prompt-chart-tick" x={PAD.left + INNER_WIDTH} y={baseline + 16} textAnchor="end">
        {formatDateTime(maxTs)}
      </text>
    </svg>
  );
}

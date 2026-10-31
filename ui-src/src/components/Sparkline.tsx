const VIEW_WIDTH = 64;
const VIEW_HEIGHT = 26;
const BAR_GAP = 2;
const MIN_BAR_HEIGHT = 2;

interface SparklineProps {
  readonly values: readonly number[];
  readonly label: string;
}

export function Sparkline({ values, label }: SparklineProps) {
  if (values.length === 0) {
    return null;
  }
  const max = Math.max(0, ...values);
  const barWidth = (VIEW_WIDTH - BAR_GAP * (values.length - 1)) / values.length;
  const lastIndex = values.length - 1;

  return (
    <svg className="sparkline" viewBox={`0 0 ${VIEW_WIDTH} ${VIEW_HEIGHT}`} role="img" aria-label={label}>
      {values.map((value, index) => {
        const height = max > 0 ? Math.max(MIN_BAR_HEIGHT, (value / max) * VIEW_HEIGHT) : MIN_BAR_HEIGHT;
        return (
          <rect
            key={index}
            className={index === lastIndex ? "spark-bar latest" : "spark-bar"}
            x={index * (barWidth + BAR_GAP)}
            y={VIEW_HEIGHT - height}
            width={barWidth}
            height={height}
            rx={1.5}
          />
        );
      })}
    </svg>
  );
}

interface ShareBarProps {
  /** Share of the whole, from 0 to 1. */
  readonly fraction: number;
  readonly label: string;
  readonly className?: string;
}

const MIN_VISIBLE_PCT = 1;

export function ShareBar({ fraction, label, className }: ShareBarProps) {
  const pct = Number.isFinite(fraction) ? Math.min(100, Math.max(0, fraction * 100)) : 0;
  const width = pct > 0 ? Math.max(MIN_VISIBLE_PCT, pct) : 0;
  return (
    <span className={className ? `share-bar ${className}` : "share-bar"} role="img" aria-label={label}>
      <span className="share-fill" style={{ width: `${width}%` }} />
    </span>
  );
}

import { useChangeFlash } from "../lib/live";
import { Sparkline } from "./Sparkline";

export type KpiTone = "default" | "info" | "violet" | "cyan" | "warn" | "danger";

interface KpiCardProps {
  readonly label: string;
  readonly value: string;
  readonly sub?: string;
  /** Oldest first; the last value is drawn as the current day. */
  readonly series?: readonly number[];
  readonly seriesLabel?: string;
  readonly tone?: KpiTone;
  readonly onClick?: () => void;
}

export function KpiCard({ label, value, sub, series, seriesLabel, tone = "default", onClick }: KpiCardProps) {
  const toneClass = tone === "default" ? "" : ` tone-${tone}`;
  const flashing = useChangeFlash(value);
  const body = (
    <>
      <span className="kpi-label">{label}</span>
      <span className="kpi-row">
        <span className={flashing ? "kpi-value flash" : "kpi-value"}>{value}</span>
        {series && series.length > 0 ? (
          <Sparkline values={series} label={seriesLabel ?? `${label}, last ${series.length} days`} />
        ) : null}
      </span>
      {sub ? <span className="kpi-sub">{sub}</span> : null}
    </>
  );

  if (onClick) {
    return (
      <button type="button" className={`kpi kpi-link${toneClass}`} onClick={onClick}>
        {body}
      </button>
    );
  }
  return <div className={`kpi${toneClass}`}>{body}</div>;
}

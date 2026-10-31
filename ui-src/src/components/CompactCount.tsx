import { formatNumber } from "../lib/format";

const COMPACT = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const COMPACT_FROM = 10_000;

interface CompactCountProps {
  readonly value: number;
}

export function CompactCount({ value }: CompactCountProps) {
  if (!Number.isFinite(value) || Math.abs(value) < COMPACT_FROM) {
    return <>{formatNumber(value)}</>;
  }
  return <span title={formatNumber(value)}>{COMPACT.format(value)}</span>;
}

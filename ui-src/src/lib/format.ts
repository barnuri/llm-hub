export function formatTime(tsMs: number): string {
  return new Date(tsMs).toLocaleTimeString();
}

/** Time only for today, otherwise short date plus time, so older rows are not ambiguous. */
export function formatDateTime(tsMs: number): string {
  const date = new Date(tsMs);
  if (date.toDateString() === new Date().toDateString()) {
    return date.toLocaleTimeString();
  }
  return `${date.toLocaleDateString(undefined, { month: "short", day: "numeric" })}, ${date.toLocaleTimeString()}`;
}

export function formatDate(tsMs: number): string {
  return new Date(tsMs).toLocaleDateString();
}

export function formatNumber(value: number, digits = 0): string {
  if (!Number.isFinite(value)) {
    return "—";
  }
  return value.toLocaleString(undefined, {
    maximumFractionDigits: digits,
    minimumFractionDigits: digits > 0 ? Math.min(digits, 1) : 0,
  });
}

export function formatMs(value: number): string {
  if (!value) {
    return "—";
  }
  if (value >= 1000) {
    return `${formatNumber(value / 1000, 2)} s`;
  }
  return `${formatNumber(value)} ms`;
}

export function formatTps(value: number): string {
  if (!value) {
    return "—";
  }
  return `${formatNumber(value, value >= 100 ? 0 : 1)} tok/s`;
}

export function formatPct(value: number): string {
  if (!Number.isFinite(value)) {
    return "—";
  }
  return `${formatNumber(value, 1)}%`;
}

export function formatUsd(value: number): string {
  if (!Number.isFinite(value) || value === 0) {
    return "$0.00";
  }
  if (value > 0 && value < 0.01) {
    return `$${value.toLocaleString(undefined, {
      minimumFractionDigits: 4,
      maximumFractionDigits: 6,
    })}`;
  }
  return `$${value.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`;
}


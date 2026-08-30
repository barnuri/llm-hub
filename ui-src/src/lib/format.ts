export function formatTime(tsMs: number): string {
  return new Date(tsMs).toLocaleTimeString();
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


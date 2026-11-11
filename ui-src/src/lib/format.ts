// Local time zone, always 24-hour: the backend sends UTC, and an en-US browser would show 12-hour.
const TIME_FORMAT = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
});
const DAY_FORMAT = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });

export function formatTime(tsMs: number): string {
  return TIME_FORMAT.format(tsMs);
}

/** Time only for today, otherwise short date plus time, so older rows are not ambiguous. */
export function formatDateTime(tsMs: number): string {
  const date = new Date(tsMs);
  if (date.toDateString() === new Date().toDateString()) {
    return TIME_FORMAT.format(date);
  }
  return `${DAY_FORMAT.format(date)}, ${TIME_FORMAT.format(date)}`;
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


export function errorText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

import type { SeriesPoint } from "./types";

export type SeriesBucket = "hour" | "day";

export type UpstreamTone = "top" | "second" | "rest";

export interface UpstreamGroup {
  readonly key: string;
  readonly label: string;
  readonly tone: UpstreamTone;
  readonly total: number;
}

export interface UpstreamBucket {
  readonly ts: number;
  /** Requests per group, in the same order as `UpstreamStacks.groups`. */
  readonly values: readonly number[];
  readonly total: number;
}

export interface UpstreamStacks {
  readonly groups: readonly UpstreamGroup[];
  readonly buckets: readonly UpstreamBucket[];
}

const NAMED_UPSTREAMS = 2;
const REST_KEY = "__rest__";

export function formatBucket(tsMs: number, bucket: SeriesBucket): string {
  const date = new Date(tsMs);
  if (bucket === "hour") {
    return date.toLocaleTimeString(undefined, { hour: "numeric" });
  }
  return date.toLocaleDateString(undefined, { weekday: "short", day: "numeric" });
}

export function uniqueTimes(series: readonly SeriesPoint[]): number[] {
  return [...new Set(series.map((point) => point.ts_ms))].sort((left, right) => left - right);
}

function upstreamOf(modelKey: string): string {
  const slash = modelKey.indexOf("/");
  return slash > 0 ? modelKey.slice(0, slash) : modelKey;
}

/** Groups the per-model series by its `<profile>/` prefix: the two busiest profiles, then everyone else. */
export function stackByUpstream(series: readonly SeriesPoint[]): UpstreamStacks {
  const totals = new Map<string, number>();
  for (const point of series) {
    const upstream = upstreamOf(point.key);
    totals.set(upstream, (totals.get(upstream) ?? 0) + point.requests);
  }
  const ranked = [...totals.entries()]
    .filter(([, total]) => total > 0)
    .sort(([leftKey, left], [rightKey, right]) => right - left || leftKey.localeCompare(rightKey));
  const named = ranked.slice(0, NAMED_UPSTREAMS);
  const rest = ranked.slice(NAMED_UPSTREAMS);

  const groups: UpstreamGroup[] = named.map(([key, total], index) => ({
    key,
    label: key,
    tone: index === 0 ? "top" : "second",
    total,
  }));
  if (rest.length > 0) {
    groups.push({
      key: REST_KEY,
      label: rest.length === 1 ? (rest[0]?.[0] ?? "other") : `${rest.length} others`,
      tone: "rest",
      total: rest.reduce((sum, [, total]) => sum + total, 0),
    });
  }

  const groupIndex = (upstream: string): number => {
    const index = groups.findIndex((group) => group.key === upstream);
    return index >= 0 ? index : groups.findIndex((group) => group.key === REST_KEY);
  };
  const buckets = uniqueTimes(series).map((ts) => {
    const values = groups.map(() => 0);
    for (const point of series) {
      if (point.ts_ms !== ts || point.requests === 0) {
        continue;
      }
      const index = groupIndex(upstreamOf(point.key));
      if (index >= 0) {
        values[index] = (values[index] ?? 0) + point.requests;
      }
    }
    return { ts, values, total: values.reduce((sum, value) => sum + value, 0) };
  });
  return { groups, buckets };
}

export interface ProfileRow {
  readonly name: string;
  /** Optional UI label; falls back to `name` when absent. */
  readonly display_name: string | null;
  /** Server-resolved label (`display_name` or `name`). */
  readonly label: string;
  readonly base_url: string;
  readonly api_key_masked: string;
  readonly headers: Record<string, string>;
  readonly timeout_ms: number | null;
  readonly enabled: boolean;
  readonly models: readonly string[];
  readonly healthy: boolean | null;
}

export interface HubMeta {
  readonly profiles: readonly ProfileRow[];
  /** Default fallback chain of model ids, tried in order on failure. */
  readonly default_fallbacks: readonly string[];
  readonly readonly: boolean;
  readonly persistent: boolean;
  readonly auth_enabled: boolean;
  readonly version: string;
}

export interface StatsEntry {
  readonly key: string;
  readonly requests: number;
  readonly errors: number;
  readonly tokens_in: number;
  readonly tokens_out: number;
  readonly cache_read_tokens: number;
  readonly cache_write_tokens: number;
  readonly p50_ms: number;
  readonly p95_ms: number;
  readonly ttft_p50_ms: number;
  readonly ttft_p95_ms: number;
  readonly tokens_per_sec_p50: number;
  readonly tokens_per_sec_avg: number;
  readonly cost_usd: number;
}

export interface StatsOverview {
  readonly requests: number;
  readonly errors: number;
  readonly tokens_in: number;
  readonly tokens_out: number;
  readonly cache_read_tokens: number;
  readonly cache_write_tokens: number;
  readonly cache_hit_rate_pct: number;
  readonly p50_ms: number;
  readonly p95_ms: number;
  readonly ttft_p50_ms: number;
  readonly ttft_p95_ms: number;
  readonly tokens_per_sec_p50: number;
  readonly tokens_per_sec_avg: number;
  readonly cost_usd: number;
}

export interface StatsPricing {
  readonly configured: boolean;
  readonly note: string;
}

export interface StatsSnapshot {
  readonly range: string;
  readonly persistent: boolean;
  readonly filterable: boolean;
  readonly overview: StatsOverview;
  readonly profiles: readonly StatsEntry[];
  readonly models: readonly StatsEntry[];
  readonly pricing: StatsPricing;
  readonly series_bucket?: string;
  readonly series?: readonly SeriesPoint[];
}

export interface SeriesPoint {
  readonly ts_ms: number;
  readonly key: string;
  readonly requests: number;
  readonly tokens_in: number;
  readonly tokens_out: number;
  readonly cache_read_tokens: number;
  readonly cache_write_tokens: number;
  readonly cost_usd: number;
}

export type StatsRange = "live" | "1d" | "7d" | "30d" | "all";

export interface UsageRow {
  readonly ts_ms: number;
  readonly model: string;
  readonly profile: string;
  readonly status: number;
  readonly latency_ms: number;
  readonly ttft_ms: number | null;
  readonly tokens_in: number;
  readonly tokens_out: number;
  readonly cache_read_tokens: number;
  readonly cache_write_tokens: number;
  readonly cost_usd: number;
}

export interface UsageReport {
  readonly total_requests: number;
  readonly total_errors: number;
  readonly total_tokens_in: number;
  readonly total_tokens_out: number;
  readonly recent: readonly UsageRow[];
}

export interface ErrorsReport {
  readonly range: string;
  readonly total_errors: number;
  readonly recent: readonly UsageRow[];
}

export type ErrorsRange = "1d" | "7d" | "30d" | "all";

export interface ApiKeyRow {
  readonly name: string;
  readonly masked: string;
  readonly enabled: boolean;
  readonly created_ms: number;
}

export interface KeysResponse {
  readonly persistent: boolean;
  readonly keys: readonly ApiKeyRow[];
}

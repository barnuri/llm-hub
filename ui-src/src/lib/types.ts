export interface ProfileRow {
  readonly name: string;
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
  readonly p50_ms: number;
  readonly p95_ms: number;
}

export interface StatsSnapshot {
  readonly profiles: readonly StatsEntry[];
  readonly models: readonly StatsEntry[];
}

export interface UsageRow {
  readonly ts_ms: number;
  readonly model: string;
  readonly profile: string;
  readonly status: number;
  readonly latency_ms: number;
  readonly tokens_in: number;
  readonly tokens_out: number;
}

export interface UsageReport {
  readonly total_requests: number;
  readonly total_errors: number;
  readonly total_tokens_in: number;
  readonly total_tokens_out: number;
  readonly recent: readonly UsageRow[];
}

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

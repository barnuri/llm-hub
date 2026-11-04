/** USD per 1M tokens, from `llm_hub_price` on a `/v1/models` entry. All zero for a local model. */
export interface HubModelPrice {
  readonly input_usd_per_1m: number;
  readonly output_usd_per_1m: number;
  readonly cache_read_usd_per_1m: number;
  readonly cache_write_usd_per_1m: number;
}

export interface HubModel {
  readonly id: string;
  readonly display_name?: string;
  readonly description?: string;
  readonly auto_tier?: AutoTier;
  readonly llm_hub_price?: HubModelPrice;
}

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
  readonly error?: string | null;
  /** Caller signature: User-Agent plus harness markers. */
  readonly client?: string | null;
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

export type AutoTurn = "routed" | "reused" | "default";

/** The Copilot Auto tier a decision was routed on. */
export type AutoTier = "efficiency" | "balance" | "intelligence" | "fast";

export interface AutoDecision {
  readonly ts_ms: number;
  readonly session: string | null;
  readonly profile: string;
  readonly auto_pick: string;
  readonly model: string;
  readonly swap_reason: string | null;
  readonly complexity: "low" | "medium" | "high" | null;
  readonly selection_reason: string | null;
  readonly turn: AutoTurn;
  readonly estimated_prompt_tokens: number;
  readonly tier: AutoTier;
}

export interface AutoDecisionsResponse {
  readonly decisions: readonly AutoDecision[];
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

export type ModelHealth = "healthy" | "degraded" | "failing" | "insufficient_data";

export type InsightsRange = "1d" | "7d" | "30d" | "all";

export interface HarnessInsight {
  readonly harness: string;
  readonly requests: number;
  readonly success_rate_pct: number;
  readonly ttft_p50_ms: number | null;
  readonly decode_tokens_per_sec_p50: number | null;
}

export interface UseCasePick {
  readonly use_case: string;
  readonly model: string;
  readonly ranked: boolean;
  readonly reason: string;
  readonly candidates: readonly string[];
}

export interface ModelInsight {
  readonly model: string;
  readonly profile: string;
  readonly context_window: number | null;
  readonly use_cases: readonly string[];
  readonly summary: string | null;
  readonly requests: number;
  readonly errors: number;
  readonly success_rate_pct: number;
  readonly consecutive_failures: number;
  readonly health: ModelHealth;
  readonly ttft_p50_ms: number | null;
  readonly ttft_p95_ms: number | null;
  readonly latency_p50_ms: number | null;
  readonly decode_tokens_per_sec_p50: number | null;
  readonly prefill_tokens_per_sec_p50: number | null;
  readonly cache_hit_rate_pct: number;
  readonly tokens_in: number;
  readonly tokens_out: number;
  readonly top_errors: ReadonlyArray<{ readonly reason: string; readonly count: number }>;
  readonly last_success_ms: number | null;
  readonly last_error_ms: number | null;
  readonly ranked: boolean;
  readonly stability_score: number | null;
  readonly speed_score: number | null;
  readonly overall_score: number | null;
  readonly by_harness: readonly HarnessInsight[];
  readonly best_harness: string | null;
}

export interface InsightLeaders {
  readonly best_overall: string | null;
  readonly most_stable: string | null;
  readonly fastest_first_token: string | null;
  readonly fastest_decode: string | null;
  readonly fastest_prefill: string | null;
  readonly most_used: string | null;
}

export interface InsightsReport {
  readonly range: string;
  readonly min_requests: number;
  readonly generated_ms: number;
  readonly leaders: InsightLeaders;
  readonly best_for: readonly UseCasePick[];
  readonly harnesses: readonly HarnessInsight[];
  readonly models: readonly ModelInsight[];
}

export interface ModelPick {
  readonly by: string;
  readonly model: string;
  readonly value: number;
  readonly reason: string;
  readonly alternatives: ReadonlyArray<{ readonly model: string; readonly value: number }>;
}

export interface CopilotQuota {
  readonly id: string;
  /** `aic` is AI credits (100 AIC = $1). */
  readonly unit: "aic" | "requests";
  readonly used: number;
  readonly entitlement: number;
  readonly remaining?: number;
  readonly remainingPercentage: number;
  readonly overagePermitted?: boolean;
  readonly resetDate?: string;
  readonly usedUsd?: number;
  readonly limitUsd?: number;
}

export interface CopilotUsageReport {
  readonly profile: string;
  readonly source: string;
  readonly updatedAt: number;
  readonly fetchedAt: string;
  readonly quotas: readonly CopilotQuota[];
}

/** Where one Smart Auto dispatch stands; a dispatch emits selected, started, then one terminal state. */
export type SelectionState = "selected" | "started" | "completed" | "rejected" | "cancelled";

type SelectionStage = "solver" | "judge" | "critic" | "repair";

type SelectionRole = "planning" | "investigation" | "implementation" | "validation" | "conversation";

type SelectionEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

/** One row of `GET /api/smart-auto/history`. Scope keys are salted hashes; no prompt or response text is present. */
export interface SelectionEvent {
  readonly seq: number;
  readonly ts_ms: number;
  readonly workflow: string | null;
  readonly worker: string | null;
  readonly attempt: string | null;
  readonly turn: string | null;
  readonly selector: string | null;
  readonly tier: string | null;
  readonly role: SelectionRole | null;
  readonly stage: SelectionStage;
  readonly model: string;
  readonly state: SelectionState;
  readonly reason: string | null;
  readonly policy_version: number;
  readonly requested_effort: SelectionEffort | null;
  readonly selected_effort: SelectionEffort | null;
  readonly sent_effort: SelectionEffort | null;
  readonly confirmed_effort: SelectionEffort | null;
  readonly status: number | null;
  readonly latency_ms: number | null;
  readonly tokens_in: number | null;
  readonly tokens_out: number | null;
  readonly cache_read_tokens: number | null;
  readonly cache_write_tokens: number | null;
}

export interface SelectionHistoryResponse {
  readonly events: readonly SelectionEvent[];
}

/** The `selection` of `GET /api/smart-auto/current`: the worker's main solver event, its active stages and freshness. */
export interface CurrentSelection {
  readonly main: SelectionEvent | null;
  readonly active: readonly SelectionEvent[];
  readonly state: SelectionState;
  readonly age_ms: number;
  readonly stale: boolean;
}

/** `GET /api/smart-auto/current?session=<id>` (or `worker=<id>`); `selection` is null until the worker has one. */
export interface CurrentSelectionResponse {
  readonly selection: CurrentSelection | null;
}

/** Include and exclude globs over "profile/model" ids; "*" matches any run of characters, including "/". */
export interface SmartAutoGlobs {
  readonly include: readonly string[];
  readonly exclude: readonly string[];
}

/** The Smart Auto rules document, from `GET` and `PUT /api/smart-auto/rules`. */
export interface SmartAutoRules extends SmartAutoGlobs {
  /** Global globs that let a model in even when an exclude or include list would keep it out. */
  readonly allow: readonly string[];
  readonly profiles: Readonly<Record<string, SmartAutoGlobs>>;
}

export type SmartAutoRulesSource = "file" | "env" | "none";

/** `GET /api/smart-auto/rules`; `readonly` is true when LLM_HUB_SMART_AUTO_RULES controls the rules. */
export interface SmartAutoRulesResponse {
  readonly source: SmartAutoRulesSource;
  readonly readonly: boolean;
  readonly rules: SmartAutoRules;
}

/** Why the saved rules let a model in (`eligible`) or keep it out. */
export type SmartAutoEffectReason =
  | "eligible"
  | "allowed_override"
  | "excluded_by_global"
  | "excluded_by_profile"
  | "not_in_global_include"
  | "not_in_profile_include"
  | "not_chat"
  | "auto_alias"
  | "hub_namespace";

/** One model from `GET /api/smart-auto/rules/effect`; `matched` is the glob that decided it, if any. */
export interface SmartAutoEffectRow {
  readonly qualified_id: string;
  readonly eligible: boolean;
  readonly reason: SmartAutoEffectReason;
  readonly matched: string | null;
}

export interface SmartAutoEffectResponse {
  readonly rows: readonly SmartAutoEffectRow[];
}

import { api } from "./api";
import { formatNumber } from "./format";
import type { KeepWarmFigures } from "./types";

export type SessionsRange = "today" | "1d" | "7d" | "30d" | "all";

export type CacheWriteCause = "first_in_session" | "model_switch" | "idle_over_ttl" | "warm" | "no_session_id";

export interface SessionPromptStats {
  readonly min: number;
  readonly median: number;
  readonly p90: number;
  readonly max: number;
  readonly peak_ts_ms: number;
}

export interface SessionSummary extends KeepWarmFigures {
  readonly worker: string;
  readonly client: string | null;
  readonly project: string | null;
  readonly requests: number;
  readonly user_turns: number;
  readonly first_ts_ms: number;
  readonly last_ts_ms: number;
  readonly duration_ms: number;
  readonly models: readonly string[];
  readonly prompt: SessionPromptStats | null;
  readonly cost_usd: number;
  readonly input_usd: number;
  readonly output_usd: number;
  readonly cache_read_usd: number;
  readonly cache_write_usd: number;
  readonly long_tier_usd: number;
  /** Fraction of `cost_usd` priced at long-context rates, from 0 to 1. */
  readonly long_tier_share: number;
  readonly compactions: number;
  /** Part of `cache_write_usd` written after the cache expired. */
  readonly idle_cache_write_usd: number;
  /** Successful requests and cost per `routed_by`, most expensive first. */
  readonly routed_by_spend: readonly SessionRoutedBySpend[];
}

/** `pinned` is the client's own model; `smart-auto:<tier>` and `copilot-auto` are hub picks; `unknown` predates the column. */
interface SessionRoutedBySpend {
  readonly routed_by: string;
  readonly requests: number;
  readonly cost_usd: number;
}

export interface SessionCacheCause {
  readonly cause: CacheWriteCause;
  readonly requests: number;
  readonly cache_write_tokens: number;
  readonly cache_write_usd: number;
}

export interface SessionList {
  readonly range: string;
  readonly sessions: readonly SessionSummary[];
  readonly no_session_id: SessionCacheCause;
}

export interface SessionPromptPoint {
  readonly ts_ms: number;
  readonly model: string;
  readonly prompt_tokens: number;
  readonly cost_usd: number;
}

export interface SessionCompaction {
  readonly ts_ms: number;
  readonly model: string;
  readonly previous_prompt_tokens: number;
  readonly prompt_tokens: number;
  readonly drop_percent: number;
}

export interface SessionToolResult {
  readonly family: string;
  readonly bytes: number;
  readonly estimated_tokens: number;
}

export interface SessionSkillBody {
  readonly name: string;
  readonly chars: number;
  readonly estimated_tokens: number;
}

/** Token figures are estimates: characters or bytes divided by four. */
export interface SessionComposition {
  readonly system_chars: number;
  readonly system_estimated_tokens: number;
  readonly tool_count: number;
  readonly tool_bytes: number;
  readonly tool_estimated_tokens: number;
  readonly mcp_tool_count: number;
  readonly mcp_tool_bytes: number;
  readonly mcp_tool_estimated_tokens: number;
  readonly deferred_listing_chars: number | null;
  readonly deferred_listing_estimated_tokens: number | null;
  readonly skills_listing_chars: number | null;
  readonly skills_listing_estimated_tokens: number | null;
  readonly claude_md_chars: number | null;
  readonly claude_md_estimated_tokens: number | null;
  readonly image_count: number;
  /** Tool-result text by tool family, largest first. */
  readonly tool_results: readonly SessionToolResult[];
  /** Skill bodies in the user messages, largest first. */
  readonly skill_bodies: readonly SessionSkillBody[];
}

export interface SessionDetail {
  readonly summary: SessionSummary;
  readonly series: readonly SessionPromptPoint[];
  readonly series_sampled: boolean;
  readonly compactions: readonly SessionCompaction[];
  readonly cache_causes: readonly SessionCacheCause[];
  readonly composition: SessionComposition | null;
}

const SHORT_WORKER_CHARS = 8;
const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
const SECOND_MS = 1_000;

export function parseSessionsRange(value: string | null): SessionsRange {
  if (value === "today" || value === "1d" || value === "30d" || value === "all") {
    return value;
  }
  return "7d";
}

/** The worker segment of `sessions/<worker>`, decoded, or null on the list page. */
export function workerFromPath(path: string): string | null {
  const [, segment] = path.split("/");
  if (!segment) {
    return null;
  }
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

export function fetchSessions(range: SessionsRange): Promise<SessionList> {
  return api<SessionList>(`/api/sessions?range=${range}`);
}

export function fetchSessionDetail(worker: string, range: SessionsRange): Promise<SessionDetail> {
  return api<SessionDetail>(`/api/sessions/${encodeURIComponent(worker)}?range=${range}`);
}

export function sessionName(session: Pick<SessionSummary, "project" | "worker">): string {
  return session.project ?? session.worker.slice(0, SHORT_WORKER_CHARS);
}

export function shareOf(part: number, whole: number): number {
  return whole > 0 ? part / whole : 0;
}

export function formatDuration(durationMs: number): string {
  if (!Number.isFinite(durationMs)) {
    return "—";
  }
  if (durationMs < MINUTE_MS) {
    return `${formatNumber(Math.round(durationMs / SECOND_MS))} s`;
  }
  if (durationMs < HOUR_MS) {
    return `${formatNumber(Math.round(durationMs / MINUTE_MS))} min`;
  }
  if (durationMs < DAY_MS) {
    const hours = Math.floor(durationMs / HOUR_MS);
    const minutes = Math.round((durationMs % HOUR_MS) / MINUTE_MS);
    return `${hours} h ${minutes} min`;
  }
  const days = Math.floor(durationMs / DAY_MS);
  const hours = Math.round((durationMs % DAY_MS) / HOUR_MS);
  return `${days} d ${hours} h`;
}

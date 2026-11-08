import type { HubModelPrice, SelectionEvent } from "./types";

const PER_MILLION = 1_000_000;

/**
 * Where one event's cost belongs. `answer` is the delivered answer. `checker` is judge or critic overhead.
 * The other three are waste: `repair` is a repair call, `failed` is a rejected or cancelled solver call,
 * and `rejected_draft` is a solver answer that a reviewer rejected.
 */
export type CostKind = "answer" | "checker" | "repair" | "failed" | "rejected_draft";

export const COST_KIND_TEXT: Readonly<Record<CostKind, string>> = {
  answer: "Answer",
  checker: "Checker",
  repair: "Repair",
  failed: "Failed call",
  rejected_draft: "Rejected draft",
};

export const WASTE_KINDS: readonly CostKind[] = ["repair", "failed", "rejected_draft"];

const FAILED_SOLVER_STATES: ReadonlySet<SelectionEvent["state"]> = new Set(["rejected", "cancelled"]);

/** The reviewer reason on a rejected judge or critic stage. The hub writes it as `draft_rejected`. */
const DRAFT_REJECTED = "draft_rejected";

/**
 * The USD cost of one event: its tokens times the model's rates, with cache reads and writes priced separately.
 * The input split matches `estimate_with_rates` on the hub. `null` when the event reports no tokens or the model has no price.
 */
export function eventCost(event: SelectionEvent, prices: ReadonlyMap<string, HubModelPrice>): number | null {
  const price = prices.get(event.model);
  if (price === undefined || (event.tokens_in === null && event.tokens_out === null)) {
    return null;
  }
  const reportedIn = event.tokens_in ?? 0;
  const cacheRead = event.cache_read_tokens ?? 0;
  const cacheWrite = event.cache_write_tokens ?? 0;
  const tokensOut = event.tokens_out ?? 0;
  // A cache read larger than the input was reported apart from it (Anthropic), so the input is not reduced.
  const uncachedIn = cacheRead > reportedIn ? reportedIn : reportedIn - cacheRead;
  const cachedIn = cacheRead;
  return (
    (uncachedIn * price.input_usd_per_1m +
      cachedIn * price.cache_read_usd_per_1m +
      cacheWrite * price.cache_write_usd_per_1m +
      tokensOut * price.output_usd_per_1m) /
    PER_MILLION
  );
}

/**
 * The kind of every event in `events`. A solver call is `rejected_draft` when a judge or critic in the same
 * attempt rejected its draft, so the solver's own tokens count as waste on that attempt.
 */
export function classifyEvents(events: readonly SelectionEvent[]): ReadonlyMap<SelectionEvent, CostKind> {
  const rejectedAttempts = new Set<string>();
  for (const event of events) {
    if (event.attempt !== null && event.reason === DRAFT_REJECTED && event.stage !== "solver") {
      rejectedAttempts.add(event.attempt);
    }
  }
  const kinds = new Map<SelectionEvent, CostKind>();
  for (const event of events) {
    kinds.set(event, kindOf(event, rejectedAttempts));
  }
  return kinds;
}

function kindOf(event: SelectionEvent, rejectedAttempts: ReadonlySet<string>): CostKind {
  if (event.stage === "judge" || event.stage === "critic") {
    return "checker";
  }
  if (event.stage === "repair") {
    return "repair";
  }
  if (FAILED_SOLVER_STATES.has(event.state)) {
    return "failed";
  }
  if (event.attempt !== null && rejectedAttempts.has(event.attempt) && event.state === "completed") {
    return "rejected_draft";
  }
  return "answer";
}

export interface CostTotals {
  /** Summed cost of the events with a known price and tokens. */
  readonly usd: number;
  /** Events with a cost. */
  readonly priced: number;
  /** Events that made a call but have no cost, because the tokens or the price are missing. */
  readonly unpriced: number;
}

export function sumCost(
  events: readonly SelectionEvent[],
  prices: ReadonlyMap<string, HubModelPrice>,
): CostTotals {
  let usd = 0;
  let priced = 0;
  let unpriced = 0;
  for (const event of events) {
    const cost = eventCost(event, prices);
    if (cost === null) {
      if (event.state === "completed") {
        unpriced += 1;
      }
      continue;
    }
    usd += cost;
    priced += 1;
  }
  return { usd, priced, unpriced };
}

#!/usr/bin/env node
// Compares prompt-cache cost before and after a cutover time from the hub's own request log.
// Usage: node scripts/cache-savings.mjs --cutover <iso|ms> [--hours 24] [--db llm-hub.db] [--prices <url|file>] [--json]
import { readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { pathToFileURL } from "node:url";

const KEEP_WARM_CLIENT = "llm-hub/keep-warm";
const LONG_CONTEXT_SUFFIX = "[1m]";
const PER_MILLION = 1_000_000;

/** Reads `--name value` pairs into an object. */
export function parseArgs(argv) {
  const args = { hours: 24, db: "llm-hub.db", prices: "http://127.0.0.1:8888/v1/models", json: false };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i].replace(/^--/, "");
    if (key === "json") args.json = true;
    else args[key] = argv[(i += 1)];
  }
  if (!args.cutover) throw new Error("--cutover is required (ISO time or epoch ms of the deploy)");
  const cutover = /^\d+$/.test(args.cutover) ? Number(args.cutover) : Date.parse(args.cutover);
  if (!Number.isFinite(cutover)) throw new Error(`bad --cutover '${args.cutover}'`);
  const hours = Number(args.hours);
  if (!(hours > 0)) throw new Error(`bad --hours '${args.hours}'`);
  return { ...args, cutover, hours };
}

/** Maps `profile/model` to the `llm_hub_price` card of a `/v1/models` body. */
export function priceTable(models) {
  const table = new Map();
  for (const entry of models.data ?? []) {
    if (!entry.llm_hub_price) continue;
    const id = entry.id.endsWith(LONG_CONTEXT_SUFFIX) ? entry.id.slice(0, -LONG_CONTEXT_SUFFIX.length) : entry.id;
    if (!table.has(id) || entry.id === id) table.set(id, entry.llm_hub_price);
  }
  return table;
}

async function loadPrices(source) {
  if (/^https?:/.test(source)) {
    const response = await fetch(source);
    if (!response.ok) throw new Error(`prices: ${source} answered ${response.status}`);
    return priceTable(await response.json());
  }
  return priceTable(JSON.parse(readFileSync(source, "utf8")));
}

/** Prices one row like the hub: `tokens_in` includes the cached part, and the tier follows the prompt size. */
export function priceRow(row, card) {
  if (!card) return { usd: 0, longTier: false };
  const prompt = Math.max(row.tokens_in, row.cache_read_tokens) + row.cache_write_tokens;
  const tier = card.long_context && prompt > card.long_context.threshold_tokens ? card.long_context : null;
  const rates = tier ?? card;
  const uncached = row.cache_write_tokens > 0 ? row.tokens_in : Math.max(0, row.tokens_in - row.cache_read_tokens);
  const usd =
    (uncached * rates.input_usd_per_1m +
      row.cache_read_tokens * rates.cache_read_usd_per_1m +
      row.cache_write_tokens * rates.cache_write_usd_per_1m +
      row.tokens_out * rates.output_usd_per_1m) /
    PER_MILLION;
  return { usd, longTier: Boolean(tier) };
}

/** Sums one window of rows in time order. A switch is a request whose model differs from the worker's last one. */
export function summarize(rows, prices) {
  const sum = {
    user_requests: 0, keep_warm_pings: 0, prompt_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0,
    cost_usd: 0, keep_warm_usd: 0, long_tier_usd: 0, long_tier_requests: 0, switches: 0, switch_uncached_tokens: 0,
  };
  const lastModel = new Map();
  for (const row of rows) {
    const { usd, longTier } = priceRow(row, prices.get(row.model));
    sum.cost_usd += usd;
    if (row.client === KEEP_WARM_CLIENT) {
      sum.keep_warm_pings += 1;
      sum.keep_warm_usd += usd;
      continue;
    }
    sum.user_requests += 1;
    sum.prompt_tokens += Math.max(row.tokens_in, row.cache_read_tokens);
    sum.cache_read_tokens += row.cache_read_tokens;
    sum.cache_write_tokens += row.cache_write_tokens;
    if (longTier) {
      sum.long_tier_usd += usd;
      sum.long_tier_requests += 1;
    }
    if (row.worker) {
      const previous = lastModel.get(row.worker);
      if (previous && previous !== row.model) {
        sum.switches += 1;
        sum.switch_uncached_tokens += Math.max(0, row.tokens_in - row.cache_read_tokens);
      }
      lastModel.set(row.worker, row.model);
    }
  }
  const perK = sum.user_requests > 0 ? 1000 / sum.user_requests : 0;
  return {
    ...sum,
    hit_rate: sum.prompt_tokens > 0 ? sum.cache_read_tokens / sum.prompt_tokens : 0,
    usd_per_1k_requests: sum.cost_usd * perK,
    long_tier_usd_per_1k: sum.long_tier_usd * perK,
    keep_warm_usd_per_1k: sum.keep_warm_usd * perK,
    switch_uncached_per_request: sum.user_requests > 0 ? sum.switch_uncached_tokens / sum.user_requests : 0,
  };
}

/** Before and after windows of equal length around the cutover, with savings scaled to the after-window traffic. */
export function compare(db, prices, cutover, hours) {
  const span = hours * 3_600_000;
  const query = db.prepare(
    `SELECT ts_ms, model, client, worker, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens
     FROM requests WHERE status BETWEEN 200 AND 299 AND ts_ms >= ? AND ts_ms < ? ORDER BY ts_ms, id`,
  );
  const before = summarize(query.all(cutover - span, cutover), prices);
  const after = summarize(query.all(cutover, cutover + span), prices);
  const savedPer1k = after.user_requests > 0 ? before.usd_per_1k_requests - after.usd_per_1k_requests : 0;
  return {
    cutover_ms: cutover,
    hours,
    before,
    after,
    saved_usd_per_1k_requests: savedPer1k,
    saved_share: before.usd_per_1k_requests > 0 ? savedPer1k / before.usd_per_1k_requests : 0,
    saved_usd_in_after_window: (savedPer1k * after.user_requests) / 1000,
  };
}

const fmt = (value, digits = 2) => (Number.isInteger(value) ? String(value) : value.toFixed(digits));

/** Renders the comparison as a Markdown table for a PR body. */
export function markdown(result) {
  const rows = [
    ["user requests", "user_requests"], ["keep-warm pings", "keep_warm_pings"], ["cache hit rate", "hit_rate"],
    ["USD total", "cost_usd"], ["USD per 1k requests", "usd_per_1k_requests"],
    ["long-tier USD per 1k", "long_tier_usd_per_1k"], ["long-tier requests", "long_tier_requests"],
    ["keep-warm USD per 1k", "keep_warm_usd_per_1k"], ["model switches", "switches"],
    ["switch uncached tokens per request", "switch_uncached_per_request"], ["cache write tokens", "cache_write_tokens"],
  ];
  const lines = [
    `Cutover ${new Date(result.cutover_ms).toISOString()}, ${result.hours} h before vs ${result.hours} h after.`,
    "",
    "| Metric | Before | After |",
    "|---|---:|---:|",
    ...rows.map(([label, key]) => `| ${label} | ${fmt(result.before[key], key === "hit_rate" ? 4 : 2)} | ${fmt(result.after[key], key === "hit_rate" ? 4 : 2)} |`),
    "",
    result.after.user_requests === 0
      ? "Saved: not measurable yet, the after window has no user requests."
      : `Saved: $${fmt(result.saved_usd_per_1k_requests)} per 1k requests (${fmt(result.saved_share * 100, 1)}%), $${fmt(result.saved_usd_in_after_window)} over the after window.`,
  ];
  return lines.join("\n");
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const prices = await loadPrices(args.prices);
  const db = new DatabaseSync(args.db, { readOnly: true });
  try {
    const result = compare(db, prices, args.cutover, args.hours);
    console.log(args.json ? JSON.stringify(result, null, 2) : markdown(result));
  } finally {
    db.close();
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  main().catch((error) => {
    console.error(error.message);
    process.exit(2);
  });
}

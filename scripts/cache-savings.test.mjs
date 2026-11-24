import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import { compare, markdown, parseArgs, priceRow, priceTable, summarize } from "./cache-savings.mjs";

const HAIKU = {
  input_usd_per_1m: 0.1, output_usd_per_1m: 0.5, cache_read_usd_per_1m: 0.01, cache_write_usd_per_1m: 0.125,
  long_context: { input_usd_per_1m: 0.5, output_usd_per_1m: 2.5, cache_read_usd_per_1m: 0.05, cache_write_usd_per_1m: 0.625, threshold_tokens: 100_000 },
};
const SONNET = { input_usd_per_1m: 2, output_usd_per_1m: 10, cache_read_usd_per_1m: 0.1, cache_write_usd_per_1m: 2.5 };
const PRICES = priceTable({
  data: [
    { id: "copilot/haiku[1m]", llm_hub_price: HAIKU },
    { id: "copilot/sonnet[1m]", llm_hub_price: SONNET },
    { id: "local/free" },
  ],
});
const row = (fields) => ({ client: "opencode", worker: "w", tokens_out: 0, cache_read_tokens: 0, cache_write_tokens: 0, ...fields });

test("priceTable strips the long-context suffix and skips unpriced models", () => {
  assert.deepEqual([...PRICES.keys()], ["copilot/haiku", "copilot/sonnet"]);
});

test("priceRow uses base rates at the threshold and long rates above it", () => {
  const base = priceRow(row({ model: "copilot/haiku", tokens_in: 100_000, cache_read_tokens: 90_000 }), HAIKU);
  assert.equal(base.longTier, false);
  assert.ok(Math.abs(base.usd - (10_000 * 0.1 + 90_000 * 0.01) / 1e6) < 1e-12);
  const long = priceRow(row({ model: "copilot/haiku", tokens_in: 120_000, cache_read_tokens: 100_000, tokens_out: 1_000 }), HAIKU);
  assert.equal(long.longTier, true);
  assert.ok(Math.abs(long.usd - (20_000 * 0.5 + 100_000 * 0.05 + 1_000 * 2.5) / 1e6) < 1e-12);
});

test("priceRow counts all input as uncached when a write is reported, and is free without a card", () => {
  const priced = priceRow(row({ tokens_in: 1_000, cache_read_tokens: 200, cache_write_tokens: 300 }), SONNET);
  assert.ok(Math.abs(priced.usd - (1_000 * 2 + 200 * 0.1 + 300 * 2.5) / 1e6) < 1e-12);
  assert.deepEqual(priceRow(row({ tokens_in: 5 }), undefined), { usd: 0, longTier: false });
});

test("summarize splits keep-warm pings and counts model switches per worker", () => {
  const sum = summarize(
    [
      row({ model: "copilot/sonnet", tokens_in: 1_000, cache_read_tokens: 900 }),
      row({ model: "copilot/haiku", tokens_in: 1_000, cache_read_tokens: 0 }),
      row({ model: "copilot/haiku", tokens_in: 1_000, cache_read_tokens: 1_000, worker: "other" }),
      row({ model: "copilot/sonnet", tokens_in: 1_000, cache_read_tokens: 1_000, client: "llm-hub/keep-warm" }),
      row({ model: "copilot/haiku", tokens_in: 10, worker: null }),
    ],
    PRICES,
  );
  assert.equal(sum.user_requests, 4);
  assert.equal(sum.keep_warm_pings, 1);
  assert.equal(sum.switches, 1);
  assert.equal(sum.switch_uncached_tokens, 1_000);
  assert.ok(Math.abs(sum.hit_rate - 1_900 / 3_010) < 1e-12);
  assert.ok(sum.keep_warm_usd > 0);
});

test("summarize of no rows is all zero", () => {
  const sum = summarize([], PRICES);
  assert.equal(sum.usd_per_1k_requests, 0);
  assert.equal(sum.hit_rate, 0);
});

test("compare reports the saving per 1k requests across the cutover", () => {
  const db = new DatabaseSync(":memory:");
  db.exec(`CREATE TABLE requests (id INTEGER PRIMARY KEY, ts_ms INTEGER, model TEXT, client TEXT, worker TEXT, status INTEGER,
    tokens_in INTEGER, tokens_out INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER)`);
  const insert = db.prepare("INSERT INTO requests (ts_ms, model, client, worker, status, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens) VALUES (?, ?, 'opencode', 'w', ?, ?, 0, ?, 0)");
  const cutover = 10 * 3_600_000;
  insert.run(cutover - 1_000, "copilot/haiku", 200, 150_000, 140_000);
  insert.run(cutover - 2_000, "copilot/haiku", 500, 150_000, 0);
  insert.run(cutover + 1_000, "copilot/sonnet", 200, 150_000, 140_000);
  const result = compare(db, PRICES, cutover, 1);
  assert.equal(result.before.long_tier_requests, 1);
  assert.equal(result.after.long_tier_requests, 0);
  const expectedBefore = (10_000 * 0.5 + 140_000 * 0.05) / 1e6 * 1000;
  const expectedAfter = (10_000 * 2 + 140_000 * 0.1) / 1e6 * 1000;
  assert.ok(Math.abs(result.saved_usd_per_1k_requests - (expectedBefore - expectedAfter)) < 1e-9);
  assert.match(markdown(result), /\| user requests \| 1 \| 1 \|/);
  const empty = compare(db, PRICES, cutover + 5 * 3_600_000, 1);
  assert.equal(empty.saved_usd_per_1k_requests, 0);
  assert.match(markdown(empty), /not measurable yet/);
});

test("parseArgs needs a valid cutover and positive hours", () => {
  assert.throws(() => parseArgs([]), /--cutover is required/);
  assert.throws(() => parseArgs(["--cutover", "nope"]), /bad --cutover/);
  assert.throws(() => parseArgs(["--cutover", "1", "--hours", "0"]), /bad --hours/);
  const args = parseArgs(["--cutover", "2026-11-24T12:00:00Z", "--hours", "6", "--json"]);
  assert.equal(args.cutover, Date.parse("2026-11-24T12:00:00Z"));
  assert.equal(args.hours, 6);
  assert.equal(args.json, true);
});

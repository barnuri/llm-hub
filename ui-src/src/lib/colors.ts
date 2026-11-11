const TAG_HUES: readonly number[] = [152, 200, 262, 32, 340, 172, 48, 218, 290, 12, 104, 320];
const HASH_SEED = 2166136261;
const HASH_PRIME = 16777619;
export const HEAT_LEVELS = 5;
const LIGHT_SATURATION = "74%";
const LIGHT_LIGHTNESS = "38%";
const DARK_SATURATION = "82%";
const DARK_LIGHTNESS = "64%";

function hashName(name: string): number {
  let hash = HASH_SEED;
  for (let index = 0; index < name.length; index += 1) {
    hash = Math.imul(hash ^ name.charCodeAt(index), HASH_PRIME);
  }
  return hash >>> 0;
}

/** A stable color for a name: the same model or upstream gets the same hue on every screen. */
export function nameColor(name: string): string {
  const hue = TAG_HUES[hashName(name) % TAG_HUES.length] ?? 152;
  return `light-dark(hsl(${hue} ${LIGHT_SATURATION} ${LIGHT_LIGHTNESS}), hsl(${hue} ${DARK_SATURATION} ${DARK_LIGHTNESS}))`;
}

/** Level 1 is the best value and level 5 the worst. Pass `good` above `bad` for a higher-is-better metric. */
export function heatLevel(value: number, good: number, bad: number): number | null {
  if (!Number.isFinite(value) || value <= 0 || good === bad) {
    return null;
  }
  const position = (value - good) / (bad - good);
  const clamped = Math.min(1, Math.max(0, position));
  return Math.min(HEAT_LEVELS, Math.floor(clamped * HEAT_LEVELS) + 1);
}

const TTFT_GOOD_MS = 800;
const TTFT_BAD_MS = 6000;
const LATENCY_GOOD_MS = 3000;
const LATENCY_BAD_MS = 30000;
const RATE_GOOD_TPS = 120;
const RATE_BAD_TPS = 15;

export function ttftHeat(ms: number): number | null {
  return heatLevel(ms, TTFT_GOOD_MS, TTFT_BAD_MS);
}

export function latencyHeat(ms: number): number | null {
  return heatLevel(ms, LATENCY_GOOD_MS, LATENCY_BAD_MS);
}

export function rateHeat(tokensPerSec: number): number | null {
  return heatLevel(tokensPerSec, RATE_GOOD_TPS, RATE_BAD_TPS);
}

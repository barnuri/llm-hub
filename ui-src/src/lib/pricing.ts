import type { HubModel, HubModelPrice } from "./types";

const LONG_CONTEXT_SUFFIX = "[1m]";
const UNKNOWN_PRICE = "—";
const FREE_PRICE = "free";

function usd(amount: number): string {
  return `$${amount.toLocaleString(undefined, { maximumFractionDigits: 3 })}`;
}

/** `$4 / $20` (USD per 1M input / output tokens), `free` when both are zero, `—` when unknown. */
export function formatPrice(price: HubModelPrice | undefined): string {
  if (price === undefined) {
    return UNKNOWN_PRICE;
  }
  if (price.input_usd_per_1m === 0 && price.output_usd_per_1m === 0) {
    return FREE_PRICE;
  }
  return `${usd(price.input_usd_per_1m)} / ${usd(price.output_usd_per_1m)}`;
}

/** Prices by qualified id. A `[1m]` id is also listed without the suffix, as rule results name it. */
export function pricesById(models: readonly HubModel[]): ReadonlyMap<string, HubModelPrice> {
  const prices = new Map<string, HubModelPrice>();
  for (const { id, llm_hub_price: price } of models) {
    if (price === undefined) {
      continue;
    }
    prices.set(id, price);
    if (id.endsWith(LONG_CONTEXT_SUFFIX)) {
      prices.set(id.slice(0, -LONG_CONTEXT_SUFFIX.length), price);
    }
  }
  return prices;
}

export type PriceSort = "none" | "asc" | "desc";

const NEXT_PRICE_SORT: Readonly<Record<PriceSort, PriceSort>> = { none: "asc", asc: "desc", desc: "none" };

/** Off → cheapest first → most expensive first → off. */
export function nextPriceSort(sort: PriceSort): PriceSort {
  return NEXT_PRICE_SORT[sort];
}

export function parsePriceSort(value: string | null): PriceSort {
  return value === "asc" || value === "desc" ? value : "none";
}

/** Input plus output USD per 1M tokens; a free model is 0. */
function priceKey(price: HubModelPrice): number {
  return price.input_usd_per_1m + price.output_usd_per_1m;
}

/** A stable copy sorted by price. Unpriced items stay last in both directions; `none` keeps the order. */
export function sortByPrice<T>(
  items: readonly T[],
  priceOf: (item: T) => HubModelPrice | undefined,
  sort: PriceSort,
): readonly T[] {
  if (sort === "none") {
    return items;
  }
  const direction = sort === "asc" ? 1 : -1;
  return [...items].sort((left, right) => {
    const leftPrice = priceOf(left);
    const rightPrice = priceOf(right);
    if (leftPrice === undefined || rightPrice === undefined) {
      return Number(leftPrice === undefined) - Number(rightPrice === undefined);
    }
    return direction * (priceKey(leftPrice) - priceKey(rightPrice));
  });
}

import { nextPriceSort, type PriceSort } from "../lib/pricing";

const ARIA_SORT: Readonly<Record<PriceSort, "ascending" | "descending" | "none">> = {
  none: "none",
  asc: "ascending",
  desc: "descending",
};

const SORT_MARK: Readonly<Record<PriceSort, string>> = { none: "↕", asc: "↑", desc: "↓" };

interface PriceHeaderProps {
  readonly sort: PriceSort;
  readonly onSort: (sort: PriceSort) => void;
}

/** The "Price / 1M" column header. A click cycles cheapest first, most expensive first, off. */
export function PriceHeader({ sort, onSort }: PriceHeaderProps) {
  return (
    <th className="num" aria-sort={ARIA_SORT[sort]}>
      <button
        type="button"
        className="th-sort"
        title="USD per 1M input / output tokens. Click to sort; free counts as $0."
        onClick={() => onSort(nextPriceSort(sort))}
      >
        Price / 1M{" "}
        <span className={sort === "none" ? "th-sort-mark dim" : "th-sort-mark"} aria-hidden="true">
          {SORT_MARK[sort]}
        </span>
      </button>
    </th>
  );
}

import { ModelName } from "./ModelName";
import "./TopList.css";

export interface TopListItem {
  readonly model: string;
  readonly value: string;
  readonly sub?: string;
}

interface TopListProps {
  readonly items: readonly TopListItem[];
  readonly empty: string;
  /** Rows shown, best first. */
  readonly limit?: number;
}

/** Up to `limit` ranked models, each with its rank, name and one value. */
export function TopList({ items, empty, limit = 3 }: TopListProps) {
  if (items.length === 0) {
    return <p className="note top-list-empty">{empty}</p>;
  }
  return (
    <ol className="top-list">
      {items.slice(0, limit).map((item, index) => (
        <li key={item.model} className="top-list-row">
          <span className={index === 0 ? "top-list-rank first" : "top-list-rank"}>{index + 1}</span>
          <span className="top-list-name">
            <ModelName id={item.model} />
            {item.sub ? <span className="top-list-sub">{item.sub}</span> : null}
          </span>
          <span className="top-list-value">{item.value}</span>
        </li>
      ))}
    </ol>
  );
}

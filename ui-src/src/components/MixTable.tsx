import { formatNumber } from "../lib/format";

export type MixTone = "top" | "second" | "rest" | "off";

export interface MixRow {
  readonly key: string;
  readonly label: string;
  readonly count: number;
  /** Bar length, 0 to 100. */
  readonly fillPct: number;
  /** Defaults to the row's rank: the largest row solid, the next darker, the rest tinted. */
  readonly tone?: MixTone;
}

interface MixTableProps {
  readonly labelHeading: string;
  readonly countHeading: string;
  readonly rows: readonly MixRow[];
}

const TONE_FILL: Readonly<Record<MixTone, string>> = {
  top: "var(--accent)",
  second: "var(--upstream-2)",
  rest: "var(--accent-tint)",
  off: "var(--border-strong)",
};

const SECOND_RANK = 1;

function rankTone(index: number): MixTone {
  if (index === 0) {
    return "top";
  }
  return index === SECOND_RANK ? "second" : "rest";
}

export function MixTable({ labelHeading, countHeading, rows }: MixTableProps) {
  return (
    <table className="table mix-table">
      <thead>
        <tr>
          <th>{labelHeading}</th>
          <th className="num">{countHeading}</th>
          <th>Share</th>
        </tr>
      </thead>
      <tbody>
        {rows.map((row, index) => {
          const fill = {
            width: `${Math.min(100, Math.max(0, row.fillPct))}%`,
            background: TONE_FILL[row.tone ?? rankTone(index)],
          };
          return (
            <tr key={row.key}>
              <td className="mono mix-label" title={row.label}>
                {row.label}
              </td>
              <td className="num">{formatNumber(row.count)}</td>
              <td className="mix-share">
                <span className="share" aria-hidden="true">
                  <span className="share-bar">
                    <span className="share-fill" style={fill} />
                  </span>
                </span>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

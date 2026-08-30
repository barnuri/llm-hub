import type { StatsEntry } from "../lib/types";

interface StatsTableProps {
  readonly label: string;
  readonly rows: readonly StatsEntry[];
}

export function StatsTable({ label, rows }: StatsTableProps) {
  return (
    <>
      <h2>{label}</h2>
      <table className="table">
        <thead>
          <tr>
            <th>{label}</th>
            <th className="num">Requests</th>
            <th className="num">Errors</th>
            <th className="num">p50 ms</th>
            <th className="num">p95 ms</th>
            <th className="num">Tokens in</th>
            <th className="num">Tokens out</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((entry) => (
            <tr key={entry.key}>
              <td className="mono">{entry.key}</td>
              <td className="num">{entry.requests}</td>
              <td className="num">{entry.errors}</td>
              <td className="num">{entry.p50_ms}</td>
              <td className="num">{entry.p95_ms}</td>
              <td className="num">{entry.tokens_in}</td>
              <td className="num">{entry.tokens_out}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </>
  );
}

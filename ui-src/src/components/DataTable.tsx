import type { ReactNode } from "react";

interface DataTableColumn {
  readonly label: string;
  readonly numeric?: boolean;
}

interface DataTableCell {
  readonly content: ReactNode;
  readonly className?: string;
}

interface DataTableRow {
  readonly key: string;
  readonly cells: readonly DataTableCell[];
}

interface DataTableProps {
  readonly columns: readonly DataTableColumn[];
  readonly rows: readonly DataTableRow[];
  /** Shown in one full-width row when `rows` is empty. */
  readonly empty: string;
  /** Extra class names for the `<table>`, beside `table`. */
  readonly tableClassName?: string;
  /** Extra class names for the scroll wrapper, beside `table-scroll`. */
  readonly wrapClassName?: string;
  readonly emptyClassName?: string;
}

/** A headed table in a scroll wrapper, with a message row for when it has no rows. */
export function DataTable({
  columns,
  rows,
  empty,
  tableClassName,
  wrapClassName,
  emptyClassName = "note",
}: DataTableProps) {
  return (
    <div className={joinClasses("table-scroll", wrapClassName)}>
      <table className={joinClasses("table", tableClassName)}>
        <thead>
          <tr>
            {columns.map((column) => (
              <th key={column.label} className={column.numeric ? "num" : undefined}>
                {column.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={columns.length} className={emptyClassName}>
                {empty}
              </td>
            </tr>
          ) : null}
          {rows.map((row) => (
            <tr key={row.key}>
              {row.cells.map((cell, index) => (
                <td key={index} className={joinClasses(columns[index]?.numeric ? "num" : undefined, cell.className)}>
                  {cell.content}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function joinClasses(...names: readonly (string | undefined)[]): string | undefined {
  const joined = names.filter((name) => name !== undefined && name !== "").join(" ");
  return joined === "" ? undefined : joined;
}

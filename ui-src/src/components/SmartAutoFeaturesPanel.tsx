import { COST_KIND_TEXT, type CostKind, eventCost } from "../lib/smartAutoCost";
import { formatNumber, formatUsd } from "../lib/format";
import type { HubModelPrice, SelectionEvent } from "../lib/types";
import { DataTable } from "./DataTable";
import { ModelTag } from "./ModelTag";
import { Panel } from "./Panel";
import { ShareBar } from "./ShareBar";
import { shortModel } from "./UsageModelChart";

const FEATURE_KINDS: readonly CostKind[] = ["checker", "repair", "failed", "rejected_draft"];

interface SmartAutoFeaturesPanelProps {
  readonly events: readonly SelectionEvent[];
  readonly kinds: ReadonlyMap<SelectionEvent, CostKind>;
  readonly prices: ReadonlyMap<string, HubModelPrice>;
}

interface ModelRow {
  readonly model: string;
  readonly calls: number;
  readonly byKind: Record<CostKind, number>;
  readonly total: number;
}

function emptyKinds(): Record<CostKind, number> {
  return { answer: 0, checker: 0, repair: 0, failed: 0, rejected_draft: 0 };
}

function rowsOf(
  events: readonly SelectionEvent[],
  kinds: ReadonlyMap<SelectionEvent, CostKind>,
  prices: ReadonlyMap<string, HubModelPrice>,
): ModelRow[] {
  const byModel = new Map<string, { calls: number; byKind: Record<CostKind, number> }>();
  for (const event of events) {
    const kind = kinds.get(event);
    if (kind === undefined || !FEATURE_KINDS.includes(kind)) {
      continue;
    }
    const entry = byModel.get(event.model) ?? { calls: 0, byKind: emptyKinds() };
    entry.calls += 1;
    entry.byKind[kind] += eventCost(event, prices) ?? 0;
    byModel.set(event.model, entry);
  }
  return [...byModel.entries()]
    .map(([model, entry]) => ({
      model,
      calls: entry.calls,
      byKind: entry.byKind,
      total: FEATURE_KINDS.reduce((sum, kind) => sum + entry.byKind[kind], 0),
    }))
    .sort((left, right) => right.total - left.total);
}

export function SmartAutoFeaturesPanel({ events, kinds, prices }: SmartAutoFeaturesPanelProps) {
  const rows = rowsOf(events, kinds, prices);
  const grand = rows.reduce((sum, row) => sum + row.total, 0);

  return (
    <Panel
      title="Smart Auto features by model"
      sub="Which models ran the reviews, repairs, failed calls and rejected drafts"
    >
      <DataTable
        columns={[
          { label: "Model" },
          { label: "Calls", numeric: true },
          { label: COST_KIND_TEXT.checker, numeric: true },
          { label: COST_KIND_TEXT.repair, numeric: true },
          { label: COST_KIND_TEXT.failed, numeric: true },
          { label: COST_KIND_TEXT.rejected_draft, numeric: true },
          { label: "Share" },
          { label: "Total", numeric: true },
        ]}
        empty="No review, repair or failed calls in this range."
        rows={rows.map((row) => ({
          key: row.model,
          cells: [
            { content: <ModelTag colorKey={shortModel(row.model)}>{row.model}</ModelTag>, className: "mono" },
            { content: formatNumber(row.calls) },
            ...(["checker", "repair", "failed", "rejected_draft"] as const).map((kind) => ({
              content: formatUsd(row.byKind[kind]),
            })),
            {
              content: (
                <span className="share">
                  <ShareBar
                    fraction={grand > 0 ? row.total / grand : 0}
                    label={`${grand > 0 ? Math.round((row.total / grand) * 100) : 0}% of features cost`}
                  />
                </span>
              ),
            },
            { content: formatUsd(row.total) },
          ],
        }))}
      />
    </Panel>
  );
}

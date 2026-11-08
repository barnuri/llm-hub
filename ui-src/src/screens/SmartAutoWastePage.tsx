import { useMemo } from "react";

import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill, type PillTone } from "../components/Pill";
import { formatDateTime, formatNumber, formatUsd } from "../lib/format";
import { classifyEvents, COST_KIND_TEXT, type CostKind, eventCost, sumCost, WASTE_KINDS } from "../lib/smartAutoCost";
import type { HubModelPrice, SelectionEvent } from "../lib/types";

const RECENT_WASTE_ROWS = 50;
const UNSET = "—";

const KIND_TONE: Readonly<Record<CostKind, PillTone>> = {
  answer: "ok",
  checker: "neutral",
  repair: "warn",
  failed: "danger",
  rejected_draft: "warn",
};

/** Kinds in the order the page lists them. Checker overhead comes last because it is not waste. */
const KIND_ORDER: readonly CostKind[] = ["rejected_draft", "repair", "failed", "checker", "answer"];

interface KindRow {
  readonly kind: CostKind;
  readonly calls: number;
  readonly usd: number;
  readonly tokensIn: number;
  readonly tokensOut: number;
}

interface ModelRow {
  readonly model: string;
  readonly wasteUsd: number;
  readonly wasteCalls: number;
  readonly totalUsd: number;
}

interface SmartAutoWastePageProps {
  readonly events: readonly SelectionEvent[];
  readonly prices: ReadonlyMap<string, HubModelPrice>;
}

/**
 * What Smart Auto spent on calls that did not deliver the answer. Waste is repair calls, failed solver calls
 * and solver answers a reviewer rejected. Checker calls (judge, critic) are shown apart: they are the cost of checking.
 */
export function SmartAutoWastePage({ events, prices }: SmartAutoWastePageProps) {
  const kinds = useMemo(() => classifyEvents(events), [events]);

  const { kindRows, modelRows, wasteUsd, totalUsd, checkerUsd, wastedCount, wastedEvents, unpriced } = useMemo(() => {
    const byKind = new Map<CostKind, { calls: number; usd: number; tokensIn: number; tokensOut: number }>();
    const byModel = new Map<string, { wasteUsd: number; wasteCalls: number; totalUsd: number }>();
    const wasted: { event: SelectionEvent; kind: CostKind; usd: number | null }[] = [];
    let waste = 0;
    let total = 0;
    let checker = 0;

    for (const event of events) {
      const kind = kinds.get(event) ?? "answer";
      const cost = eventCost(event, prices);
      const row = byKind.get(kind) ?? { calls: 0, usd: 0, tokensIn: 0, tokensOut: 0 };
      row.calls += 1;
      row.usd += cost ?? 0;
      row.tokensIn += event.tokens_in ?? 0;
      row.tokensOut += event.tokens_out ?? 0;
      byKind.set(kind, row);

      const model = byModel.get(event.model) ?? { wasteUsd: 0, wasteCalls: 0, totalUsd: 0 };
      model.totalUsd += cost ?? 0;
      total += cost ?? 0;
      if (kind === "checker") {
        checker += cost ?? 0;
      }
      if (WASTE_KINDS.includes(kind)) {
        model.wasteUsd += cost ?? 0;
        model.wasteCalls += 1;
        waste += cost ?? 0;
        wasted.push({ event, kind, usd: cost });
      }
      byModel.set(event.model, model);
    }

    const kindRowsOut: KindRow[] = KIND_ORDER.flatMap((kind) => {
      const row = byKind.get(kind);
      return row === undefined
        ? []
        : [{ kind, calls: row.calls, usd: row.usd, tokensIn: row.tokensIn, tokensOut: row.tokensOut }];
    });
    const modelRowsOut: ModelRow[] = [...byModel.entries()]
      .map(([model, row]) => ({ model, ...row }))
      .sort((a, b) => b.wasteUsd - a.wasteUsd || b.totalUsd - a.totalUsd);

    return {
      kindRows: kindRowsOut,
      modelRows: modelRowsOut,
      wasteUsd: waste,
      totalUsd: total,
      checkerUsd: checker,
      wastedCount: wasted.length,
      wastedEvents: wasted.slice(-RECENT_WASTE_ROWS).reverse(),
      unpriced: sumCost(events, prices).unpriced,
    };
  }, [events, kinds, prices]);

  const share = totalUsd > 0 ? `${((wasteUsd / totalUsd) * 100).toFixed(2)}% of cost` : "No cost yet";

  return (
    <>
      <div className="smart-auto-kpis">
        <KpiCard
          label="Waste"
          value={formatUsd(wasteUsd)}
          sub={share}
          tone={wasteUsd > 0 ? "danger" : "default"}
        />
        <KpiCard label="Total cost" value={formatUsd(totalUsd)} sub={`${formatNumber(unpriced)} calls unpriced`} />
        <KpiCard label="Checker cost" value={formatUsd(checkerUsd)} sub="Judge and critic calls, not waste" />
        <KpiCard label="Wasted calls" value={formatNumber(wastedCount)} sub="In the events this tab holds" />
      </div>

      <Panel title="By kind" sub="Every call, grouped by what it did" className="smart-auto-panel">
        <div className="table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th>Kind</th>
                <th className="num">Calls</th>
                <th className="num">Cost</th>
                <th className="num">Tokens in / out</th>
              </tr>
            </thead>
            <tbody>
              {kindRows.length === 0 ? (
                <tr>
                  <td colSpan={4} className="table-empty">
                    No calls yet.
                  </td>
                </tr>
              ) : (
                kindRows.map((row) => (
                  <tr key={row.kind}>
                    <td data-label="Kind">
                      <Pill tone={KIND_TONE[row.kind]}>{COST_KIND_TEXT[row.kind]}</Pill>
                    </td>
                    <td className="num" data-label="Calls">
                      {formatNumber(row.calls)}
                    </td>
                    <td className="num" data-label="Cost">
                      {formatUsd(row.usd)}
                    </td>
                    <td className="num" data-label="Tokens in / out">
                      {`${formatNumber(row.tokensIn)} / ${formatNumber(row.tokensOut)}`}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>

      <Panel title="By model" sub="Waste per model, largest first" className="smart-auto-panel">
        <div className="table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th>Model</th>
                <th className="num">Wasted calls</th>
                <th className="num">Waste</th>
                <th className="num">Total cost</th>
              </tr>
            </thead>
            <tbody>
              {modelRows.length === 0 ? (
                <tr>
                  <td colSpan={4} className="table-empty">
                    No calls yet.
                  </td>
                </tr>
              ) : (
                modelRows.map((row) => (
                  <tr key={row.model}>
                    <td className="mono" data-label="Model">
                      {row.model}
                    </td>
                    <td className="num" data-label="Wasted calls">
                      {formatNumber(row.wasteCalls)}
                    </td>
                    <td className="num" data-label="Waste">
                      {formatUsd(row.wasteUsd)}
                    </td>
                    <td className="num" data-label="Total cost">
                      {formatUsd(row.totalUsd)}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>

      <Panel title="Wasted calls" sub={`Newest ${RECENT_WASTE_ROWS}`} className="smart-auto-panel">
        <div className="table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Stage</th>
                <th>Model</th>
                <th>Kind</th>
                <th>Reason</th>
                <th className="num">Cost</th>
              </tr>
            </thead>
            <tbody>
              {wastedEvents.length === 0 ? (
                <tr>
                  <td colSpan={6} className="table-empty">
                    No waste in the events this tab holds.
                  </td>
                </tr>
              ) : (
                wastedEvents.map(({ event, kind, usd }) => (
                  <tr key={event.seq}>
                    <td data-label="Time">{formatDateTime(event.ts_ms)}</td>
                    <td data-label="Stage">{event.stage}</td>
                    <td className="mono" data-label="Model">
                      {event.model}
                    </td>
                    <td data-label="Kind">
                      <Pill tone={KIND_TONE[kind]}>{COST_KIND_TEXT[kind]}</Pill>
                    </td>
                    <td className="smart-auto-reason" data-label="Reason">
                      {event.reason ?? UNSET}
                    </td>
                    <td className="num" data-label="Cost">
                      {usd === null ? UNSET : formatUsd(usd)}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </Panel>
    </>
  );
}

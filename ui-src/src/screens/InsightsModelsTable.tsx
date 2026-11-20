import { useMemo, useState, type ReactNode } from "react";

import { ModelName } from "../components/ModelName";
import { Pill, type PillTone } from "../components/Pill";
import { Segmented, type SegmentedOption } from "../components/Segmented";
import { formatCompact, formatMs, formatNumber, formatPct, formatUsd } from "../lib/format";
import type { ModelHealth, ModelInsight } from "../lib/types";

const HEALTH_LABEL: Readonly<Record<ModelHealth, string>> = {
  healthy: "Healthy",
  degraded: "Degraded",
  failing: "Failing now",
  insufficient_data: "Too few calls",
};

const HEALTH_TONE: Readonly<Record<ModelHealth, PillTone>> = {
  healthy: "ok",
  degraded: "warn",
  failing: "danger",
  insufficient_data: "neutral",
};

type HealthFilter = "all" | ModelHealth;

const HEALTH_OPTIONS: readonly SegmentedOption<HealthFilter>[] = [
  { value: "all", label: "All" },
  { value: "healthy", label: "Healthy" },
  { value: "degraded", label: "Degraded" },
  { value: "failing", label: "Failing" },
  { value: "insufficient_data", label: "Too few calls" },
];

export function HealthPill({ health }: { readonly health: ModelHealth }) {
  return <Pill tone={HEALTH_TONE[health]}>{HEALTH_LABEL[health]}</Pill>;
}

type SortValue = string | number | null;

interface Column {
  readonly key: string;
  readonly label: string;
  readonly title?: string;
  readonly numeric: boolean;
  readonly value: (model: ModelInsight) => SortValue;
  readonly render: (model: ModelInsight, copy: (text: string) => void) => ReactNode;
}

function formatContext(tokens: number | null): string {
  if (!tokens) {
    return "—";
  }
  if (tokens >= 1_000_000) {
    return `${formatNumber(tokens / 1_000_000, 1)}M`;
  }
  const unit = tokens % 1024 === 0 ? 1024 : 1000;
  return `${formatNumber(Math.round(tokens / unit))}k`;
}

function formatRate(tokensPerSec: number | null): string {
  if (!tokensPerSec) {
    return "—";
  }
  return formatNumber(tokensPerSec, tokensPerSec >= 100 ? 0 : 1);
}

function dash(value: string | null | undefined): ReactNode {
  return value ? value : <span className="dim">—</span>;
}

const COLUMNS: readonly Column[] = [
  {
    key: "model",
    label: "Model",
    numeric: false,
    value: (m) => m.model,
    render: (m, copy) => (
      <span className={m.ranked ? "mono nowrap" : "mono nowrap insights-unranked"} title={m.summary ?? m.model}>
        <ModelName id={m.model} onCopy={copy} />
        {m.free ? (
          <span className="insights-free-tag">
            <Pill tone="ok">Free</Pill>
          </span>
        ) : null}
      </span>
    ),
  },
  {
    key: "use_cases",
    label: "Good for",
    numeric: false,
    value: (m) => m.use_cases.join(", ") || null,
    render: (m) =>
      m.use_cases.length > 0 ? (
        m.use_cases.map((useCase) => (
          <span key={useCase} className="insights-use">
            <Pill>{useCase}</Pill>
          </span>
        ))
      ) : (
        <span className="dim">—</span>
      ),
  },
  {
    key: "health",
    label: "Health",
    numeric: false,
    value: (m) => HEALTH_LABEL[m.health],
    render: (m) => <HealthPill health={m.health} />,
  },
  {
    key: "context_window",
    label: "Ctx",
    title: "Context window",
    numeric: true,
    value: (m) => m.context_window,
    render: (m) => formatContext(m.context_window),
  },
  {
    key: "requests",
    label: "Calls",
    numeric: true,
    value: (m) => m.requests,
    render: (m) => formatCompact(m.requests),
  },
  {
    key: "success_rate_pct",
    label: "Success",
    numeric: true,
    value: (m) => m.success_rate_pct,
    render: (m) => formatPct(m.success_rate_pct),
  },
  {
    key: "ttft_p50_ms",
    label: "TTFT p50",
    title: "Time to first token, median",
    numeric: true,
    value: (m) => m.ttft_p50_ms,
    render: (m) => <span className="nowrap">{formatMs(m.ttft_p50_ms ?? 0)}</span>,
  },
  {
    key: "ttft_p95_ms",
    label: "TTFT p95",
    title: "Time to first token, slow tail",
    numeric: true,
    value: (m) => m.ttft_p95_ms,
    render: (m) => <span className="nowrap">{formatMs(m.ttft_p95_ms ?? 0)}</span>,
  },
  {
    key: "decode_tokens_per_sec_p50",
    label: "Out tok/s",
    title: "Output speed, median",
    numeric: true,
    value: (m) => m.decode_tokens_per_sec_p50,
    render: (m) => <span className="nowrap">{formatRate(m.decode_tokens_per_sec_p50)}</span>,
  },
  {
    key: "prefill_tokens_per_sec_p50",
    label: "Prefill tok/s",
    title: "Prompt reading speed, median",
    numeric: true,
    value: (m) => m.prefill_tokens_per_sec_p50,
    render: (m) => <span className="nowrap">{formatRate(m.prefill_tokens_per_sec_p50)}</span>,
  },
  {
    key: "cache_hit_rate_pct",
    label: "Cache",
    title: "Prompt share served from cache",
    numeric: true,
    value: (m) => m.cache_hit_rate_pct,
    render: (m) => formatPct(m.cache_hit_rate_pct),
  },
  {
    key: "cost_usd",
    label: "Cost",
    title: "Estimated spend in the window",
    numeric: true,
    value: (m) => (m.free ? 0 : m.cost_usd),
    render: (m) => (m.free ? "$0.00" : m.cost_usd === null ? "—" : formatUsd(m.cost_usd)),
  },
  {
    key: "value_score",
    label: "Value",
    title: "Score per US cent per successful call",
    numeric: true,
    value: (m) => m.value_score,
    render: (m) => (m.value_score === null ? "—" : formatCompact(m.value_score)),
  },
  {
    key: "verified_pass_pct",
    label: "Verified",
    title: "Pass share of tests, reviews and user verdicts on Smart Auto attempts",
    numeric: true,
    value: (m) => m.verified_pass_pct,
    render: (m) =>
      m.verified_pass_pct === null
        ? "—"
        : `${formatPct(m.verified_pass_pct)} of ${formatCompact(m.verified_outcomes)}`,
  },
  {
    key: "sessions_solved",
    label: "Sessions",
    title: "Verified sessions this model led: solved / verified",
    numeric: true,
    value: (m) => (m.sessions_verified > 0 ? m.sessions_solved / m.sessions_verified : null),
    render: (m) => (m.sessions_verified > 0 ? `${m.sessions_solved}/${m.sessions_verified}` : "—"),
  },
  {
    key: "quality_score",
    label: "Quality",
    title: "Public capability tier of the model family, 0-100",
    numeric: true,
    value: (m) => m.quality_score,
    render: (m) => (m.quality_score === null ? "—" : formatNumber(m.quality_score, 0)),
  },
  {
    key: "overall_score",
    label: "Overall",
    numeric: true,
    value: (m) => m.overall_score,
    render: (m) => (m.overall_score === null ? "—" : formatNumber(m.overall_score, 1)),
  },
  {
    key: "best_harness",
    label: "Best harness",
    numeric: false,
    value: (m) => m.best_harness,
    render: (m) => <span className="mono nowrap">{dash(m.best_harness)}</span>,
  },
  {
    key: "top_error",
    label: "Most common error",
    numeric: false,
    value: (m) => m.top_errors[0]?.reason ?? null,
    render: (m) => {
      const topError = m.top_errors[0];
      return topError ? (
        <span className="mono insights-top-error" title={topError.reason}>
          {`${topError.reason} (×${topError.count})`}
        </span>
      ) : (
        <span className="dim">None</span>
      );
    },
  },
];

interface Sort {
  readonly key: string | null;
  readonly dir: "asc" | "desc";
}

/** Nulls sort last in either direction. */
function compareValues(a: SortValue, b: SortValue, dir: "asc" | "desc"): number {
  if (a === null && b === null) {
    return 0;
  }
  if (a === null) {
    return 1;
  }
  if (b === null) {
    return -1;
  }
  const order = typeof a === "number" && typeof b === "number" ? a - b : String(a).localeCompare(String(b));
  return dir === "asc" ? order : -order;
}

export function InsightsModelsTable({
  models,
  onCopy,
}: {
  readonly models: readonly ModelInsight[];
  readonly onCopy: (text: string) => void;
}) {
  const [text, setText] = useState("");
  const [health, setHealth] = useState<HealthFilter>("all");
  const [freeOnly, setFreeOnly] = useState(false);
  const [sort, setSort] = useState<Sort>({ key: null, dir: "desc" });

  const rows = useMemo(() => {
    const needle = text.trim().toLowerCase();
    const filtered = models.filter((model) => {
      if (health !== "all" && model.health !== health) {
        return false;
      }
      if (freeOnly && !model.free) {
        return false;
      }
      if (needle === "") {
        return true;
      }
      return model.model.toLowerCase().includes(needle) || model.use_cases.some((u) => u.includes(needle));
    });
    if (sort.key === null) {
      return filtered;
    }
    const column = COLUMNS.find((candidate) => candidate.key === sort.key);
    if (!column) {
      return filtered;
    }
    return [...filtered].sort((a, b) => compareValues(column.value(a), column.value(b), sort.dir));
  }, [models, text, health, freeOnly, sort]);

  const clickHeader = (column: Column) => {
    setSort((current) => {
      if (current.key !== column.key) {
        return { key: column.key, dir: column.numeric ? "desc" : "asc" };
      }
      return { key: column.key, dir: current.dir === "asc" ? "desc" : "asc" };
    });
  };

  return (
    <>
      <div className="insights-filters" role="search">
        <label className="insights-filter-text">
          <span className="insights-filter-label">Filter</span>
          <input
            type="search"
            className="insights-input"
            placeholder="Model or use case"
            value={text}
            onChange={(event) => setText(event.target.value)}
          />
        </label>
        <Segmented label="Filter by health" options={HEALTH_OPTIONS} value={health} onChange={setHealth} />
        <button
          type="button"
          className="btn small"
          aria-pressed={freeOnly}
          onClick={() => setFreeOnly((value) => !value)}
        >
          Free only
        </button>
        <span className="insights-count dim" aria-live="polite">
          {formatNumber(rows.length)} of {formatNumber(models.length)} models
        </span>
      </div>
      <div className="table-scroll insights-table-scroll">
        <table className="table insights-models">
          <thead>
            <tr>
              {COLUMNS.map((column) => {
                const active = sort.key === column.key;
                return (
                  <th
                    key={column.key}
                    className={column.numeric ? "num" : undefined}
                    title={column.title}
                    aria-sort={active ? (sort.dir === "asc" ? "ascending" : "descending") : "none"}
                  >
                    <button type="button" className="insights-sort" onClick={() => clickHeader(column)}>
                      {column.label}
                      <span className="insights-sort-mark" aria-hidden="true">
                        {active ? (sort.dir === "asc" ? "▲" : "▼") : ""}
                      </span>
                    </button>
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody>
            {rows.map((model) => (
              <tr key={model.model}>
                {COLUMNS.map((column) => (
                  <td key={column.key} className={column.numeric ? "num" : undefined}>
                    {column.render(model, onCopy)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {rows.length === 0 ? <p className="note insights-empty">No model matches these filters.</p> : null}
    </>
  );
}

import { useEffect, useState } from "react";

import { MarkdownText } from "../components/MarkdownText";
import { Panel } from "../components/Panel";
import { Select } from "../components/Select";
import { api } from "../lib/api";
import { formatCompact, formatUsd } from "../lib/format";

interface AnalysisCandidate {
  readonly model: string;
  readonly quality_score: number | null;
  readonly free: boolean;
  readonly estimated_cost_usd: number;
  readonly recommended: boolean;
  readonly reason: string;
}

interface AnalysisModels {
  readonly estimated_input_tokens: number;
  readonly max_output_tokens: number;
  readonly expected_output_tokens: number;
  readonly fair_run_usd: number;
  readonly candidates: readonly AnalysisCandidate[];
}

interface AnalysisResult {
  readonly model: string;
  readonly markdown: string;
  readonly cost_usd: number;
  readonly latency_ms: number;
}

function costLabel(candidate: AnalysisCandidate): string {
  return candidate.free ? "free" : `~${formatUsd(candidate.estimated_cost_usd)}`;
}

function optionLabel(candidate: AnalysisCandidate): string {
  const parts = [candidate.model, costLabel(candidate)];
  if (candidate.quality_score !== null) {
    parts.push(`quality ${Math.round(candidate.quality_score)}`);
  }
  if (candidate.recommended) {
    parts.push("recommended");
  }
  return parts.join(" · ");
}

/** A manual, one-click AI review of the deterministic insights. It spends tokens only on click. */
export function InsightsAnalysis({ query }: { readonly query: string }) {
  const [models, setModels] = useState<AnalysisModels | null>(null);
  const [model, setModel] = useState("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<AnalysisResult | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api<AnalysisModels>(`/api/insights/analysis/models?${query}`)
      .then((body) => {
        if (!cancelled) {
          setModels(body);
          setModel((current) => current || body.candidates[0]?.model || "");
        }
      })
      .catch(() => {
        if (!cancelled) {
          setModels(null);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [query]);

  const selected = models?.candidates.find((candidate) => candidate.model === model);
  const run = () => {
    setRunning(true);
    setError(null);
    api<AnalysisResult>(`/api/insights/analysis?${query}`, {
      method: "POST",
      body: JSON.stringify({ model }),
    })
      .then(setResult)
      .catch((err: unknown) => setError(err instanceof Error ? err.message : String(err)))
      .finally(() => setRunning(false));
  };

  if (!models || models.candidates.length === 0) {
    return null;
  }
  return (
    <Panel
      title="AI analysis"
      sub="optional · one model call on click · the scores above never use AI"
      className="insights-analysis"
    >
      <div className="toolbar insights-analysis-controls">
        <Select
          ariaLabel="Model for the analysis"
          value={model}
          options={models.candidates.map((candidate) => ({ value: candidate.model, label: optionLabel(candidate) }))}
          onChange={setModel}
          className="insights-analysis-model"
        />
        <button type="button" className="btn small" onClick={run} disabled={running || !model}>
          {running ? "Analyzing…" : "Analyze"}
        </button>
      </div>
      {selected ? (
        <p className="dim insights-panel-note">
          {selected.reason}. Sends about {formatCompact(models.estimated_input_tokens)} tokens and expects about{" "}
          {formatCompact(models.expected_output_tokens)} back (at most {formatCompact(models.max_output_tokens)}).
          Recommended is the best quality at or below {formatUsd(models.fair_run_usd)} per run.
        </p>
      ) : null}
      {error ? (
        <p className="note" role="alert">
          {error}
        </p>
      ) : null}
      {result ? (
        <div role="status">
          <p className="dim insights-panel-note">
            {result.model} · {formatUsd(result.cost_usd)} · {Math.round(result.latency_ms / 1000)} s
          </p>
          <MarkdownText markdown={result.markdown} />
        </div>
      ) : null}
    </Panel>
  );
}

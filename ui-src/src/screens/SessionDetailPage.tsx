import { useEffect, useState } from "react";

import { DataTable } from "../components/DataTable";
import { KpiCard } from "../components/KpiCard";
import { ModelName } from "../components/ModelName";
import { Panel } from "../components/Panel";
import { PromptSizeChart } from "../components/PromptSizeChart";
import { ShareBar } from "../components/ShareBar";
import { errorText, formatDateTime, formatNumber, formatPct, formatUsd } from "../lib/format";
import { navigate } from "../lib/router";
import {
  type CacheWriteCause,
  fetchSessionDetail,
  formatDuration,
  type SessionCacheCause,
  type SessionComposition,
  type SessionDetail,
  type SessionsRange,
  type SessionSkillBody,
  type SessionSummary,
  type SessionToolResult,
  sessionName,
  shareOf,
} from "../lib/sessions";

interface SessionDetailPageProps {
  readonly worker: string;
  readonly range: SessionsRange;
}

interface ShareRow {
  readonly label: string;
  readonly value: string;
  readonly fraction: number;
  readonly title?: string;
}

const CAUSE_LABELS: Readonly<Record<CacheWriteCause, string>> = {
  first_in_session: "First request in session",
  model_switch: "Model switch",
  idle_over_ttl: "Idle past cache TTL",
  warm: "Warm cache",
  no_session_id: "No session id",
};

export function SessionDetailPage({ worker, range }: SessionDetailPageProps) {
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [unavailable, setUnavailable] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchSessionDetail(worker, range)
      .then((body) => {
        if (!cancelled) {
          setUnavailable(null);
          setDetail(body);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setDetail(null);
          setUnavailable(errorText(err));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [worker, range]);

  const back = (
    <button type="button" className="btn small" onClick={() => navigate(`/sessions?range=${range}`)}>
      Back to sessions
    </button>
  );

  if (unavailable) {
    return (
      <section className="tab active sessions-detail">
        <div className="toolbar">{back}</div>
        <Panel title="Session unavailable">
          <p className="note" role="status">
            {unavailable}
          </p>
        </Panel>
      </section>
    );
  }

  if (!detail) {
    return (
      <section className="tab active sessions-detail" aria-busy="true">
        <div className="toolbar">{back}</div>
        <p className="note">Loading…</p>
      </section>
    );
  }

  const summary = detail.summary;
  return (
    <section className="tab active sessions-detail">
      <div className="toolbar sessions-detail-head">
        {back}
        <span className="sessions-detail-name mono" title={summary.worker}>
          {sessionName(summary)}
        </span>
        <span className="dim">
          {summary.client ?? "Unknown client"} · {formatDateTime(summary.first_ts_ms)}
        </span>
      </div>

      <SessionKpis summary={summary} />

      <Panel
        title="Prompt size per request"
        sub={`${formatNumber(detail.series.length)} requests${detail.series_sampled ? ", sampled" : ""}`}
      >
        <PromptSizeChart series={detail.series} compactions={detail.compactions} />
        {detail.series_sampled ? (
          <p className="dim sessions-panel-note">The peak and every compaction are kept when the series is sampled.</p>
        ) : null}
      </Panel>

      <div className="sessions-split">
        <CostPanel summary={summary} />
        <CacheCausesPanel causes={detail.cache_causes} />
      </div>

      <RoutedByPanel rows={summary.routed_by_spend} />

      <KeepWarmCard summary={summary} />

      <CompactionsPanel detail={detail} />

      <CompositionPanel composition={detail.composition} />

      {detail.composition ? (
        <div className="sessions-split">
          <ToolResultsPanel rows={detail.composition.tool_results} />
          <SkillBodiesPanel rows={detail.composition.skill_bodies} />
        </div>
      ) : null}
    </section>
  );
}

function SessionKpis({ summary }: { readonly summary: SessionSummary }) {
  const prompt = summary.prompt;
  return (
    <div className="kpis sessions-kpis">
      <KpiCard label="Cost" value={formatUsd(summary.cost_usd)} sub={`${formatNumber(summary.requests)} requests`} />
      <KpiCard
        label="Duration"
        value={formatDuration(summary.duration_ms)}
        sub={`${formatNumber(summary.user_turns)} user turns`}
      />
      <KpiCard
        label="Prompt median"
        value={prompt ? formatNumber(prompt.median) : "—"}
        sub={prompt ? `p90 ${formatNumber(prompt.p90)} · peak ${formatNumber(prompt.max)}` : "No prompt sizes"}
      />
      <KpiCard label="Compactions" value={formatNumber(summary.compactions)} sub="prompt drops" />
      <KpiCard
        label="Long tier"
        value={formatPct(summary.long_tier_share * 100)}
        sub={`${formatUsd(summary.long_tier_usd)} at long-context rates`}
      />
    </div>
  );
}

function CostPanel({ summary }: { readonly summary: SessionSummary }) {
  const rows: readonly ShareRow[] = [
    { label: "Input", value: formatUsd(summary.input_usd), fraction: shareOf(summary.input_usd, summary.cost_usd) },
    { label: "Output", value: formatUsd(summary.output_usd), fraction: shareOf(summary.output_usd, summary.cost_usd) },
    {
      label: "Cache read",
      value: formatUsd(summary.cache_read_usd),
      fraction: shareOf(summary.cache_read_usd, summary.cost_usd),
    },
    {
      label: "Cache write",
      value: formatUsd(summary.cache_write_usd),
      fraction: shareOf(summary.cache_write_usd, summary.cost_usd),
    },
  ];
  return (
    <Panel title="Cost by component" sub={formatUsd(summary.cost_usd)}>
      <ShareRows rows={rows} />
      <p className="dim sessions-panel-note">
        Idle cache write is part of cache write: {formatUsd(summary.idle_cache_write_usd)}. Long-context rates add{" "}
        {formatUsd(summary.long_tier_usd)} ({formatPct(summary.long_tier_share * 100)} of cost), already included in the
        figures above.
      </p>
    </Panel>
  );
}

function CacheCausesPanel({ causes }: { readonly causes: readonly SessionCacheCause[] }) {
  return (
    <Panel title="Cache-write causes" sub="why a request wrote to the prompt cache">
      <div className="table-scroll">
        <table className="table sessions-causes">
          <thead>
            <tr>
              <th>Cause</th>
              <th className="num">Requests</th>
              <th className="num">Cache write tokens</th>
              <th className="num">Cache write</th>
            </tr>
          </thead>
          <tbody>
            {causes.length === 0 ? (
              <tr>
                <td colSpan={4} className="note sessions-empty">
                  No cache writes in this session.
                </td>
              </tr>
            ) : null}
            {causes.map((cause) => (
              <tr key={cause.cause}>
                <td>{CAUSE_LABELS[cause.cause]}</td>
                <td className="num">{formatNumber(cause.requests)}</td>
                <td className="num">{formatNumber(cause.cache_write_tokens)}</td>
                <td className="num nowrap">{formatUsd(cause.cache_write_usd)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}

function KeepWarmCard({ summary }: { readonly summary: SessionSummary }) {
  return (
    <Panel title="Keep-warm" sub="cache pings for idle Claude streams">
      <dl className="sessions-keepwarm">
        <dt>Pings</dt>
        <dd>{formatNumber(summary.keepwarm_pings)}</dd>
        <dt>Ping cost</dt>
        <dd>{formatUsd(summary.keepwarm_cost_usd)}</dd>
        <dt>Returns read from cache</dt>
        <dd>{formatNumber(summary.idle_returns_cached)}</dd>
        <dt>Write cost avoided (est.)</dt>
        <dd>{formatUsd(summary.est_write_saved_usd)}</dd>
      </dl>
      <p className="dim sessions-panel-note">
        Pings are not counted as requests. The avoided write cost is an estimate from the listed prices.
      </p>
    </Panel>
  );
}

function RoutedByPanel({ rows }: { readonly rows: SessionSummary["routed_by_spend"] }) {
  return (
    <Panel title="Spend by routing" sub="successful requests, by how the model was chosen">
      <DataTable
        tableClassName="sessions-routed-by"
        columns={[{ label: "Routed by" }, { label: "Requests", numeric: true }, { label: "Cost", numeric: true }]}
        rows={rows.map((row) => ({
          key: row.routed_by,
          cells: [
            { content: row.routed_by, className: "mono" },
            { content: formatNumber(row.requests) },
            { content: formatUsd(row.cost_usd), className: "nowrap" },
          ],
        }))}
        empty="No successful requests in this session."
        emptyClassName="note sessions-empty"
      />
      <p className="dim sessions-panel-note">
        <code>pinned</code> is the model the client named. <code>smart-auto</code> and <code>copilot-auto</code> are hub
        picks. <code>unknown</code> rows predate the column.
      </p>
    </Panel>
  );
}

function ToolResultsPanel({ rows }: { readonly rows: readonly SessionToolResult[] }) {
  return (
    <Panel title="Tool results" sub="text in the newest prompt, by tool family">
      <DataTable
        tableClassName="sessions-tool-results"
        columns={[{ label: "Family" }, { label: "Bytes", numeric: true }, { label: "Est. tokens", numeric: true }]}
        rows={rows.map((row) => ({
          key: row.family,
          cells: [
            { content: row.family, className: "mono" },
            { content: formatNumber(row.bytes) },
            { content: formatNumber(row.estimated_tokens) },
          ],
        }))}
        empty="No tool results in the newest prompt."
        emptyClassName="note sessions-empty"
      />
    </Panel>
  );
}

function SkillBodiesPanel({ rows }: { readonly rows: readonly SessionSkillBody[] }) {
  return (
    <Panel title="Skill bodies" sub="skills injected into the newest prompt">
      <DataTable
        tableClassName="sessions-skill-bodies"
        columns={[{ label: "Skill" }, { label: "Chars", numeric: true }, { label: "Est. tokens", numeric: true }]}
        rows={rows.map((row) => ({
          key: row.name,
          cells: [
            { content: row.name, className: "mono" },
            { content: formatNumber(row.chars) },
            { content: formatNumber(row.estimated_tokens) },
          ],
        }))}
        empty="No skill bodies in the newest prompt."
        emptyClassName="note sessions-empty"
      />
    </Panel>
  );
}

function CompactionsPanel({ detail }: { readonly detail: SessionDetail }) {
  return (
    <Panel title="Compactions" sub="a prompt that dropped sharply from the request before it">
      {detail.compactions.length === 0 ? (
        <p className="note sessions-empty">No compactions in this session.</p>
      ) : (
        <div className="table-scroll">
          <table className="table sessions-compactions">
            <thead>
              <tr>
                <th>Time</th>
                <th>Model</th>
                <th className="num">Prompt before</th>
                <th className="num">Prompt after</th>
                <th className="num">Drop</th>
              </tr>
            </thead>
            <tbody>
              {detail.compactions.map((compaction) => (
                <tr key={`${compaction.ts_ms}-${compaction.prompt_tokens}`}>
                  <td className="nowrap">{formatDateTime(compaction.ts_ms)}</td>
                  <td className="mono nowrap"><ModelName id={compaction.model} /></td>
                  <td className="num">{formatNumber(compaction.previous_prompt_tokens)}</td>
                  <td className="num">{formatNumber(compaction.prompt_tokens)}</td>
                  <td className="num">{formatPct(compaction.drop_percent)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

function CompositionPanel({ composition }: { readonly composition: SessionComposition | null }) {
  if (!composition) {
    return (
      <Panel title="Context composition">
        <p className="note sessions-empty">No prompt shape recorded for this session yet.</p>
      </Panel>
    );
  }
  const mcpTokens = composition.mcp_tool_estimated_tokens;
  const rows: readonly TokenRow[] = [
    {
      label: "System prompt",
      tokens: composition.system_estimated_tokens,
      detail: `${formatNumber(composition.system_chars)} chars`,
    },
    {
      label: "Tools (without MCP)",
      tokens: Math.max(0, composition.tool_estimated_tokens - mcpTokens),
      detail: `${formatNumber(Math.max(0, composition.tool_count - composition.mcp_tool_count))} tools`,
    },
    {
      label: "MCP tools",
      tokens: mcpTokens,
      detail: `${formatNumber(composition.mcp_tool_count)} tools`,
    },
    {
      label: "Deferred tool listing",
      tokens: composition.deferred_listing_estimated_tokens,
      detail: composition.deferred_listing_chars === null ? "not found" : "listing",
    },
    {
      label: "Skills listing",
      tokens: composition.skills_listing_estimated_tokens,
      detail: composition.skills_listing_chars === null ? "not found" : "listing",
    },
    {
      label: "CLAUDE.md",
      tokens: composition.claude_md_estimated_tokens,
      detail: composition.claude_md_chars === null ? "not found" : "memory files",
    },
  ];
  const total = rows.reduce((sum, row) => sum + (row.tokens ?? 0), 0);
  const shareRows: readonly ShareRow[] = rows.map((row) => ({
    label: row.label,
    value: row.tokens === null ? "—" : `~${formatNumber(row.tokens)} tokens`,
    fraction: shareOf(row.tokens ?? 0, total),
    title: row.detail,
  }));
  return (
    <Panel
      title="Context composition"
      sub="estimated tokens, characters or bytes divided by four"
      actions={<span className="dim">Images: {formatNumber(composition.image_count)}</span>}
    >
      <ShareRows rows={shareRows} />
      <p className="dim sessions-panel-note">
        Listings and CLAUDE.md are found in the prompt text, so they can overlap the system prompt. Shares are relative
        to the sum of the rows.
      </p>
    </Panel>
  );
}

interface TokenRow {
  readonly label: string;
  readonly tokens: number | null;
  readonly detail: string;
}

function ShareRows({ rows }: { readonly rows: readonly ShareRow[] }) {
  return (
    <div className="sessions-share-rows">
      {rows.map((row) => (
        <div key={row.label} className="sessions-share-row" title={row.title}>
          <span className="sessions-share-label">{row.label}</span>
          <ShareBar
            fraction={row.fraction}
            label={`${row.label}: ${formatPct(row.fraction * 100)}`}
            className="sessions-share-bar"
          />
          <span className="num nowrap sessions-share-value">{row.value}</span>
        </div>
      ))}
    </div>
  );
}

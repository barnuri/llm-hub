import { useEffect, useMemo, useRef, useState } from "react";

import { CopilotPricesPanel } from "../components/CopilotPricesPanel";
import { MixTable, type MixRow } from "../components/MixTable";
import { Panel } from "../components/Panel";
import { PriceHeader } from "../components/PriceHeader";
import { Pill, type PillTone } from "../components/Pill";
import { formatNumber } from "../lib/format";
import { formatPrice, parsePriceSort, sortByPrice } from "../lib/pricing";
import { setQueryParam, useRoute } from "../lib/router";
import type { HubMeta, HubModel, ProfileRow } from "../lib/types";
import "../styles/screens/models.css";

const COPIED_MS = 1500;

interface ModelsScreenProps {
  readonly models: readonly HubModel[];
  readonly profiles: readonly ProfileRow[];
  readonly meta: HubMeta | null;
  readonly onCopy: (id: string) => void;
  readonly onSaved: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

interface UpstreamState {
  readonly tone: PillTone;
  readonly label: string;
}

function profileOf(id: string): string {
  return id.split("/")[0] ?? "";
}

function upstreamState(profile: ProfileRow): UpstreamState {
  if (!profile.enabled) {
    return { tone: "neutral", label: "disabled" };
  }
  if (profile.healthy === true) {
    return { tone: "ok", label: "up" };
  }
  return profile.healthy === false ? { tone: "danger", label: "down" } : { tone: "neutral", label: "unknown" };
}

export function ModelsScreen({ models, profiles, meta, onCopy, onSaved, onToast }: ModelsScreenProps) {
  const route = useRoute();
  const activeProfile = route.query.get("profile") ?? "";
  const query = route.query.get("q") ?? "";
  const priceSort = parsePriceSort(route.query.get("sort"));
  const urlQuery = route.query.toString();
  const [copiedId, setCopiedId] = useState<string | null>(null);
  const copiedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (copiedTimer.current !== null) {
        clearTimeout(copiedTimer.current);
      }
    },
    [],
  );

  const modelCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const { id } of models) {
      const profile = profileOf(id);
      if (profile !== "") {
        counts.set(profile, (counts.get(profile) ?? 0) + 1);
      }
    }
    return counts;
  }, [models]);

  const profileNames = useMemo(() => [...modelCounts.keys()].sort(), [modelCounts]);

  const labelFor = (name: string) => profiles.find((p) => p.name === name)?.label ?? name;

  const filtered = useMemo(
    () =>
      sortByPrice(
        models
          .filter(({ id }) => activeProfile === "" || id.startsWith(`${activeProfile}/`))
          .filter(({ id, description }) => `${id} ${description ?? ""}`.toLowerCase().includes(query.toLowerCase())),
        (model) => model.llm_hub_price,
        priceSort,
      ),
    [models, query, activeProfile, priceSort],
  );

  const byProfile: readonly MixRow[] = useMemo(() => {
    const ranked = [...modelCounts.entries()].sort(
      ([leftName, left], [rightName, right]) => right - left || leftName.localeCompare(rightName),
    );
    const most = ranked[0]?.[1] ?? 0;
    return ranked.map(([name, count]) => ({
      key: name,
      label: name,
      count,
      fillPct: most > 0 ? (count / most) * 100 : 0,
    }));
  }, [modelCounts]);

  const upstreams = useMemo(
    () =>
      [...profiles].sort(
        (left, right) =>
          (modelCounts.get(right.name) ?? 0) - (modelCounts.get(left.name) ?? 0) || left.name.localeCompare(right.name),
      ),
    [profiles, modelCounts],
  );

  const copy = (id: string) => {
    onCopy(id);
    setCopiedId(id);
    if (copiedTimer.current !== null) {
      clearTimeout(copiedTimer.current);
    }
    copiedTimer.current = setTimeout(() => setCopiedId(null), COPIED_MS);
  };

  const emptyText =
    models.length === 0 ? "No models yet. Add a profile to list its models." : "No models match this filter.";
  const profileCount = profileNames.length;

  return (
    <section className="tab active models">
      <div className="models-head">
        <span className="page-sub mono">
          {formatNumber(models.length)} available · {formatNumber(profileCount)} profile{profileCount === 1 ? "" : "s"}
        </span>
        <span className="dim models-hint">
          ids are what clients send as <code>model</code>
        </span>
      </div>

      <div className="models-grid">
        <Panel className="models-list">
          <div className="models-toolbar">
            {profileNames.length > 1 ? (
              <div className="segmented models-profiles" role="group" aria-label="Filter by profile">
                <button
                  type="button"
                  className="segmented-option"
                  aria-pressed={activeProfile === ""}
                  onClick={() => setQueryParam("profile", null)}
                >
                  All <span className="seg-count">{formatNumber(models.length)}</span>
                </button>
                {profileNames.map((name) => (
                  <button
                    key={name}
                    type="button"
                    className="segmented-option"
                    aria-pressed={activeProfile === name}
                    onClick={() => setQueryParam("profile", activeProfile === name ? null : name)}
                  >
                    {labelFor(name)} <span className="seg-count">{formatNumber(modelCounts.get(name) ?? 0)}</span>
                  </button>
                ))}
              </div>
            ) : null}
            <input
              className="input search models-search"
              type="search"
              placeholder="Filter models…"
              aria-label="Filter models"
              value={query}
              onChange={(event) => setQueryParam("q", event.target.value)}
            />
          </div>
          {filtered.length === 0 ? (
            <p className="note models-empty">{emptyText}</p>
          ) : (
            <div className="table-scroll models-scroll">
              <table className="table">
                <thead>
                  <tr>
                    <th>Model id</th>
                    <th className="models-profile-col">Profile</th>
                    <PriceHeader
                      sort={priceSort}
                      onSort={(next) => setQueryParam("sort", next === "none" ? null : next)}
                    />
                    <th className="models-actions">
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {filtered.map(({ id, auto_tier, description, llm_hub_price }) => {
                    const profile = profileOf(id);
                    const copied = copiedId === id;
                    return (
                      <tr key={id}>
                        <td>
                          <span className="model-id">
                            <span className="model-profile-part">{profile}/</span>
                            {id.slice(profile.length + 1)}
                          </span>
                          {auto_tier && description ? <p className="model-description dim">{description}</p> : null}
                        </td>
                        <td className="dim">{labelFor(profile)}</td>
                        <td className="num price">{formatPrice(llm_hub_price)}</td>
                        <td className="models-actions">
                          <button
                            type="button"
                            className={copied ? "btn small models-copy copied" : "btn small models-copy"}
                            aria-label={`Copy ${id}`}
                            onClick={() => copy(id)}
                          >
                            {copied ? "Copied" : "Copy"}
                          </button>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
          <div className="models-foot">
            <span>
              Showing {formatNumber(filtered.length)} of {formatNumber(models.length)}
            </span>
            <span>
              Filter and profile stay in the URL{urlQuery ? ": " : ""}
              {urlQuery ? <span className="mono">?{urlQuery}</span> : null}
            </span>
          </div>
        </Panel>

        <div className="models-side">
          {meta !== null && profiles.some((profile) => profile.copilot) ? (
            <CopilotPricesPanel
              key={meta.copilot_prices_refresh_secs}
              refreshSecs={meta.copilot_prices_refresh_secs}
              readonly={meta.readonly}
              onSaved={onSaved}
              onToast={onToast}
            />
          ) : null}
          {byProfile.length > 0 ? (
            <Panel title="By profile" sub="models listed">
              <MixTable labelHeading="Profile" countHeading="Models" rows={byProfile} />
            </Panel>
          ) : null}
          {upstreams.length > 0 ? (
            <Panel title="Upstream status" sub="per profile">
              <table className="table models-upstreams">
                <tbody>
                  {upstreams.map((profile) => {
                    const state = upstreamState(profile);
                    return (
                      <tr key={profile.name}>
                        <td className="mono">{profile.label}</td>
                        <td className="models-actions">
                          <Pill tone={state.tone}>{state.label}</Pill>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
              <p className="dim models-side-note">
                Models from a down upstream stay listed; requests to them fail or go to the fallback chain.
              </p>
            </Panel>
          ) : null}
        </div>
      </div>
    </section>
  );
}

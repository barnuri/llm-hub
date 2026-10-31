import { useState } from "react";

import { FallbacksEditor } from "../components/FallbacksEditor";
import { InlineConfirm } from "../components/InlineConfirm";
import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { ProfileFormPanel } from "../components/ProfileFormPanel";
import { api } from "../lib/api";
import { formatNumber } from "../lib/format";
import type { HubMeta, ProfileRow } from "../lib/types";
import "../styles/screens/profiles.css";

interface ProfilesScreenProps {
  readonly meta: HubMeta | null;
  readonly models: readonly string[];
  readonly onChanged: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

interface TestResult {
  readonly ok: boolean;
  readonly latency_ms?: number;
  readonly status?: number;
  readonly error?: string;
}

interface FormTarget {
  readonly existing: ProfileRow | null;
}

const KPI_COUNT = 4;
const TOP_PREFIXES = 2;

function healthPill(profile: ProfileRow) {
  if (!profile.enabled) {
    return <Pill>Disabled</Pill>;
  }
  if (profile.healthy === false) {
    return <Pill tone="danger">Unreachable</Pill>;
  }
  return <Pill tone="ok">Enabled</Pill>;
}

function modelCounts(models: readonly string[]): ReadonlyMap<string, number> {
  const counts = new Map<string, number>();
  for (const id of models) {
    const slash = id.indexOf("/");
    if (slash > 0) {
      const prefix = id.slice(0, slash);
      counts.set(prefix, (counts.get(prefix) ?? 0) + 1);
    }
  }
  return counts;
}

function testSummary(result: TestResult): string {
  if (result.ok) {
    return `${formatNumber(result.latency_ms ?? 0)} ms`;
  }
  return result.error ?? `status ${result.status}`;
}

function LastTest({ result }: { readonly result: TestResult | undefined }) {
  if (!result) {
    return <span className="dim" title="Not tested in this session">—</span>;
  }
  const summary = testSummary(result);
  return (
    <span className={result.ok ? undefined : "t-err"} title={summary}>
      {summary}
    </span>
  );
}

function routedSub(counts: ReadonlyMap<string, number>): string | undefined {
  const top = [...counts.entries()].sort(([, left], [, right]) => right - left).slice(0, TOP_PREFIXES);
  return top.length > 0 ? top.map(([prefix, count]) => `${formatNumber(count)} ${prefix}`).join(" · ") : undefined;
}

interface ProfileKpisProps {
  readonly profiles: readonly ProfileRow[];
  readonly models: readonly string[];
  readonly counts: ReadonlyMap<string, number>;
}

function ProfileKpis({ profiles, models, counts }: ProfileKpisProps) {
  const enabled = profiles.filter((profile) => profile.enabled);
  const reachable = enabled.filter((profile) => profile.healthy === true);
  const unreachable = enabled.filter((profile) => profile.healthy === false);
  const disabledCount = profiles.length - enabled.length;

  return (
    <div className="kpi-strip profiles-kpis">
      <KpiCard label="Profiles" value={formatNumber(profiles.length)} sub={`${formatNumber(disabledCount)} disabled`} />
      <KpiCard
        label="Enabled"
        value={formatNumber(enabled.length)}
        sub={`${formatNumber(reachable.length)} reachable now`}
      />
      <KpiCard
        label="Unreachable"
        value={formatNumber(unreachable.length)}
        sub={unreachable.length > 0 ? unreachable.map((profile) => profile.label).join(", ") : "None"}
        tone={unreachable.length > 0 ? "danger" : "default"}
      />
      <KpiCard label="Models routed" value={formatNumber(models.length)} sub={routedSub(counts)} />
    </div>
  );
}

export function ProfilesScreen({ meta, models, onChanged, onToast }: ProfilesScreenProps) {
  const [formTarget, setFormTarget] = useState<FormTarget | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [tests, setTests] = useState<Readonly<Record<string, TestResult>>>({});
  const [testing, setTesting] = useState<ReadonlySet<string>>(() => new Set());
  const readonly = meta?.readonly ?? false;
  const profiles = meta?.profiles ?? [];
  const counts = modelCounts(models);
  const exampleModel = models.find((id) => id.includes("/")) ?? "<name>/<model>";

  const remove = async () => {
    if (!deleting) {
      return;
    }
    try {
      await api(`/api/profiles/${encodeURIComponent(deleting)}`, { method: "DELETE" });
      onToast("Profile deleted");
      if (formTarget?.existing?.name === deleting) {
        setFormTarget(null);
      }
      setDeleting(null);
      onChanged();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    }
  };

  const setTestingFlag = (name: string, active: boolean) => {
    setTesting((current) => {
      const next = new Set(current);
      if (active) {
        next.add(name);
      } else {
        next.delete(name);
      }
      return next;
    });
  };

  const test = async (name: string) => {
    setTestingFlag(name, true);
    try {
      const result = await api<TestResult>(`/api/profiles/${encodeURIComponent(name)}/test`, { method: "POST" });
      setTests((current) => ({ ...current, [name]: result }));
      onToast(`${name}: ${result.ok ? `reachable (${testSummary(result)})` : testSummary(result)}`, !result.ok);
      onChanged();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setTestingFlag(name, false);
    }
  };

  const formKey = formTarget?.existing ? `edit:${formTarget.existing.name}` : "add";

  return (
    <section className="tab active profiles">
      <div className="profiles-toolbar">
        <span className="page-sub mono">{meta ? `${formatNumber(profiles.length)} configured` : ""}</span>
        <div className="profiles-toolbar-actions">
          {readonly ? (
            <Pill tone="warn" dot title="Edits are disabled">
              Read-only
            </Pill>
          ) : null}
          <button
            type="button"
            className="btn primary"
            onClick={() => setFormTarget({ existing: null })}
            disabled={readonly}
          >
            Add profile
          </button>
        </div>
      </div>

      {meta === null ? (
        <div className="kpi-strip profiles-kpis" aria-busy="true">
          {Array.from({ length: KPI_COUNT }, (_, index) => (
            <div key={index} className="skeleton kpi-skeleton" />
          ))}
        </div>
      ) : (
        <ProfileKpis profiles={profiles} models={models} counts={counts} />
      )}

      <Panel title="Upstream profiles" sub={`Name becomes the model prefix, e.g. ${exampleModel}`}>
        {meta === null ? (
          <div className="skeleton profiles-skeleton" aria-label="Loading profiles" />
        ) : profiles.length === 0 ? (
          <p className="note profiles-empty">No profiles yet. Add one to route models through an upstream.</p>
        ) : (
          <div className="table-scroll profiles-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Label</th>
                  <th>Base URL</th>
                  <th>Key</th>
                  <th className="num">Models</th>
                  <th>Status</th>
                  <th className="num">Last test</th>
                  <th>
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {profiles.map((profile) => {
                  const routed = counts.get(profile.name) ?? 0;
                  const busy = testing.has(profile.name);
                  return (
                    <tr key={profile.name}>
                      <td className="mono">{profile.name}</td>
                      <td>{profile.label}</td>
                      <td className="mono dim profiles-url" title={profile.base_url}>
                        {profile.base_url}
                      </td>
                      <td className="mono dim">{profile.api_key_masked || "—"}</td>
                      <td className={routed === 0 ? "num dim" : "num"}>{formatNumber(routed)}</td>
                      <td>{healthPill(profile)}</td>
                      <td className="num profiles-last-test">
                        <LastTest result={tests[profile.name]} />
                      </td>
                      <td className="profiles-actions">
                        <button
                          type="button"
                          className="btn small"
                          aria-label={`Test ${profile.name}`}
                          disabled={busy}
                          onClick={() => test(profile.name)}
                        >
                          {busy ? "Testing…" : "Test"}
                        </button>
                        <button
                          type="button"
                          className="btn small"
                          aria-label={`Edit ${profile.name}`}
                          onClick={() => setFormTarget({ existing: profile })}
                          disabled={readonly}
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          className="btn small danger"
                          aria-label={`Delete ${profile.name}`}
                          onClick={() => setDeleting(profile.name)}
                          disabled={readonly}
                        >
                          Delete
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      {deleting ? (
        <InlineConfirm
          key={deleting}
          title={
            <>
              Delete profile <span className="mono">{deleting}</span>?
            </>
          }
          confirmLabel="Delete profile"
          onConfirm={remove}
          onCancel={() => setDeleting(null)}
        >
          Removes it and its env vars from .env. Models under <span className="mono">{deleting}/</span> stop
          resolving immediately.
        </InlineConfirm>
      ) : null}

      <div className="profiles-split">
        <FallbacksEditor
          key={(meta?.default_fallbacks ?? []).join(",")}
          chain={meta?.default_fallbacks ?? []}
          models={models}
          readonly={readonly}
          onSaved={onChanged}
          onToast={onToast}
        />
        {formTarget ? (
          <ProfileFormPanel
            key={formKey}
            existing={formTarget.existing}
            onSaved={() => {
              setFormTarget(null);
              onChanged();
            }}
            onCancel={() => setFormTarget(null)}
            onToast={onToast}
          />
        ) : (
          <Panel title="Profile details" className="profile-form-idle">
            <p className="note">Choose Edit on a profile to change it, or Add profile to create one.</p>
          </Panel>
        )}
      </div>
    </section>
  );
}

import { type FormEvent, useCallback, useEffect, useState } from "react";

import { InlineConfirm } from "../components/InlineConfirm";
import { KpiCard } from "../components/KpiCard";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { api } from "../lib/api";
import { formatDate, formatNumber } from "../lib/format";
import type { ApiKeyRow, KeysResponse } from "../lib/types";
import "../styles/screens/keys.css";

interface KeysScreenProps {
  readonly onToast: (message: string, isError?: boolean) => void;
  /** Omit when the caller does not know the hub auth state; the Auth card is then hidden. */
  readonly authEnabled?: boolean;
}

const SPARK_DAYS = 7;
const DAY_MS = 86_400_000;
const KPI_COUNT = 3;
const PERSISTENT_ENV = "LLM_HUB_PERSISTENT=true";

function keysOverTime(keys: readonly ApiKeyRow[], now: number): number[] {
  const today = Math.floor(now / DAY_MS);
  return Array.from({ length: SPARK_DAYS }, (_, index) => {
    const dayEnd = (today - (SPARK_DAYS - 1 - index) + 1) * DAY_MS;
    return keys.filter((key) => key.created_ms < dayEnd).length;
  });
}

export function KeysScreen({ onToast, authEnabled }: KeysScreenProps) {
  const [data, setData] = useState<KeysResponse | null>(null);
  const [loadedAt, setLoadedAt] = useState(0);
  const [newName, setNewName] = useState("");
  const [nameError, setNameError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = useCallback(() => {
    api<KeysResponse>("/api/keys")
      .then((body) => {
        setData(body);
        setLoadedAt(Date.now());
      })
      .catch((err) => onToast(err instanceof Error ? err.message : String(err), true));
  }, [onToast]);

  useEffect(load, [load]);

  const create = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (newName.trim() === "") {
      setNameError("Enter a name for the key.");
      return;
    }
    setCreating(true);
    try {
      const result = await api<{ name: string; key: string }>("/api/keys", {
        method: "POST",
        body: JSON.stringify({ name: newName.trim() }),
      });
      setNewName("");
      setNameError(null);
      setRevealed(result.key);
      load();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setCreating(false);
    }
  };

  const revoke = async () => {
    if (!revoking) {
      return;
    }
    try {
      await api(`/api/keys/${encodeURIComponent(revoking)}`, { method: "DELETE" });
      onToast("Key revoked");
      setRevoking(null);
      load();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    }
  };

  const copyRevealed = (key: string) => {
    navigator.clipboard
      .writeText(key)
      .then(() => onToast("Key copied"))
      .catch(() => onToast("Clipboard unavailable", true));
  };

  const persistent = data?.persistent ?? false;
  const keys = data?.keys ?? [];

  return (
    <section className="tab active keys">
      {data === null ? (
        <div className="kpi-strip keys-kpis" aria-busy="true">
          {Array.from({ length: KPI_COUNT }, (_, index) => (
            <div key={index} className="skeleton kpi-skeleton" />
          ))}
        </div>
      ) : (
        <div className="kpi-strip keys-kpis">
          <KpiCard
            label="Hub API keys"
            value={formatNumber(keys.length)}
            sub="Hashes stored, never the key"
            series={keys.length > 0 ? keysOverTime(keys, loadedAt) : undefined}
            seriesLabel={`Hub API keys, last ${SPARK_DAYS} days`}
          />
          {authEnabled === undefined ? null : (
            <KpiCard
              label="Auth"
              value={authEnabled ? "On" : "Off"}
              sub={authEnabled ? "Clients must send a key" : "Keys are checked once auth is on"}
            />
          )}
          <KpiCard
            label="Persistence"
            value={persistent ? "On" : "Off"}
            sub={persistent ? PERSISTENT_ENV : `Set ${PERSISTENT_ENV} to manage keys`}
            tone={persistent ? "default" : "warn"}
          />
        </div>
      )}

      {revealed ? (
        <Panel title="Key created, copy it now" sub="Shown once, only its hash is stored" className="keys-reveal">
          <div className="keys-reveal-row">
            <code className="keys-reveal-value">{revealed}</code>
            <button type="button" className="btn small" onClick={() => copyRevealed(revealed)}>
              Copy
            </button>
            <button type="button" className="btn small primary" onClick={() => setRevealed(null)}>
              Done
            </button>
          </div>
        </Panel>
      ) : null}

      <Panel
        title="Hub API keys"
        sub={data ? `${keys.length} ${keys.length === 1 ? "key" : "keys"}` : undefined}
        actions={
          data && !persistent ? (
            <Pill tone="warn" dot>
              Persistence off
            </Pill>
          ) : undefined
        }
      >
        {data && !persistent ? (
          <p className="note keys-note">
            Set <code>{PERSISTENT_ENV}</code> to manage hub API keys.
          </p>
        ) : null}
        <form className="toolbar keys-create" onSubmit={create} noValidate>
          <label className="dim" htmlFor="f-keyname">
            Key name
          </label>
          <div className="keys-create-field">
            <input
              id="f-keyname"
              className="input"
              placeholder="my-laptop"
              value={newName}
              disabled={!persistent}
              aria-invalid={nameError !== null}
              aria-describedby={nameError ? "f-keyname-error" : undefined}
              onChange={(event) => {
                setNewName(event.target.value);
                setNameError(null);
              }}
            />
            {nameError ? (
              <p id="f-keyname-error" className="field-error" role="alert">
                {nameError}
              </p>
            ) : null}
          </div>
          <button type="submit" className="btn primary" disabled={!persistent || creating}>
            {creating ? "Creating…" : "Create key"}
          </button>
          <span className="dim keys-create-hint">Clients send it as the API key or bearer token.</span>
        </form>
        {data === null ? (
          <div className="skeleton keys-skeleton" aria-label="Loading keys" />
        ) : keys.length === 0 ? (
          <p className="note keys-empty">No hub API keys yet.</p>
        ) : (
          <div className="table-scroll keys-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Key</th>
                  <th>Created</th>
                  <th>
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {keys.map((key) => (
                  <tr key={key.name}>
                    <td className="mono">{key.name}</td>
                    <td className="mono dim">{key.masked}</td>
                    <td className="mono nowrap">{formatDate(key.created_ms)}</td>
                    <td className="keys-actions">
                      <button
                        type="button"
                        className="btn small danger"
                        aria-label={`Revoke key ${key.name}`}
                        onClick={() => setRevoking(key.name)}
                      >
                        Revoke
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      {revoking ? (
        <InlineConfirm
          key={revoking}
          title={
            <>
              Revoke key <span className="mono">{revoking}</span>?
            </>
          }
          confirmLabel="Revoke key"
          onConfirm={revoke}
          onCancel={() => setRevoking(null)}
        >
          Clients using it get 401 immediately.
        </InlineConfirm>
      ) : null}
    </section>
  );
}

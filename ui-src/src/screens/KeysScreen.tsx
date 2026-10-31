import { useCallback, useEffect, useState } from "react";

import { Modal } from "../components/Modal";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { api } from "../lib/api";
import { formatDate } from "../lib/format";
import type { KeysResponse } from "../lib/types";
import "../styles/screens/keys.css";

interface KeysScreenProps {
  readonly onToast: (message: string, isError?: boolean) => void;
}

export function KeysScreen({ onToast }: KeysScreenProps) {
  const [data, setData] = useState<KeysResponse | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [nameError, setNameError] = useState<string | null>(null);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = useCallback(() => {
    api<KeysResponse>("/api/keys")
      .then(setData)
      .catch((err) => onToast(err instanceof Error ? err.message : String(err), true));
  }, [onToast]);

  useEffect(load, [load]);

  const closeCreate = () => {
    setCreating(false);
    setNameError(null);
  };

  const create = async () => {
    if (newName.trim() === "") {
      setNameError("Enter a name for the key.");
      return;
    }
    try {
      const result = await api<{ name: string; key: string }>("/api/keys", {
        method: "POST",
        body: JSON.stringify({ name: newName.trim() }),
      });
      closeCreate();
      setNewName("");
      setRevealed(result.key);
      load();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
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

  const persistent = data?.persistent ?? false;
  const keys = data?.keys ?? [];

  return (
    <section className="tab active keys">
      <Panel
        title="Hub API keys"
        sub={data ? `${keys.length} ${keys.length === 1 ? "key" : "keys"}` : undefined}
        actions={
          <>
            {data && !persistent ? (
              <Pill tone="warn" dot>
                Persistence off
              </Pill>
            ) : null}
            <button type="button" className="btn primary" onClick={() => setCreating(true)} disabled={!persistent}>
              Create key
            </button>
          </>
        }
      >
        {data && !persistent ? (
          <p className="note keys-note">
            Set <code>LLM_HUB_PERSISTENT=true</code> to manage hub API keys.
          </p>
        ) : null}
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

      {creating ? (
        <Modal title="Create hub API key" confirmLabel="Create key" onConfirm={create} onCancel={closeCreate}>
          <div className="field">
            <label htmlFor="f-keyname">Key name</label>
            <input
              id="f-keyname"
              className="input"
              placeholder="my-laptop"
              value={newName}
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
        </Modal>
      ) : null}

      {revealed ? (
        <Modal title="Key created — copy it now" confirmLabel="Done" onConfirm={() => setRevealed(null)} onCancel={() => setRevealed(null)}>
          <p>This key is shown once. Only its hash is stored.</p>
          <div className="reveal">{revealed}</div>
        </Modal>
      ) : null}

      {revoking ? (
        <Modal title={`Revoke key ${revoking}`} confirmLabel="Revoke key" danger onConfirm={revoke} onCancel={() => setRevoking(null)}>
          <p>
            Clients using <b className="mono">{revoking}</b> get 401 immediately.
          </p>
        </Modal>
      ) : null}
    </section>
  );
}

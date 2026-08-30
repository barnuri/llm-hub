import { useCallback, useEffect, useState } from "react";

import { Modal } from "../components/Modal";
import { api } from "../lib/api";
import { formatDate } from "../lib/format";
import type { KeysResponse } from "../lib/types";

interface KeysScreenProps {
  readonly onToast: (message: string, isError?: boolean) => void;
}

export function KeysScreen({ onToast }: KeysScreenProps) {
  const [data, setData] = useState<KeysResponse | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [revealed, setRevealed] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<string | null>(null);

  const load = useCallback(() => {
    api<KeysResponse>("/api/keys")
      .then(setData)
      .catch((err) => onToast(err instanceof Error ? err.message : String(err), true));
  }, [onToast]);

  useEffect(load, [load]);

  const create = async () => {
    try {
      const result = await api<{ name: string; key: string }>("/api/keys", {
        method: "POST",
        body: JSON.stringify({ name: newName.trim() }),
      });
      setCreating(false);
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

  return (
    <section className="tab active">
      <div className="toolbar">
        <button type="button" className="btn primary" onClick={() => setCreating(true)} disabled={!persistent}>
          Create key
        </button>
      </div>
      {!persistent ? (
        <p className="note">
          Persistence is off. Set <code>LLM_HUB_PERSISTENT=true</code> to manage hub API keys.
        </p>
      ) : null}
      <table className="table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Key</th>
            <th>Created</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {(data?.keys ?? []).map((key) => (
            <tr key={key.name}>
              <td className="mono">{key.name}</td>
              <td className="mono">{key.masked}</td>
              <td className="mono">{formatDate(key.created_ms)}</td>
              <td>
                <button type="button" className="btn small danger" onClick={() => setRevoking(key.name)}>
                  Revoke
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {creating ? (
        <Modal title="Create hub API key" confirmLabel="Create key" onConfirm={create} onCancel={() => setCreating(false)}>
          <div className="field">
            <label htmlFor="f-keyname">Key name</label>
            <input
              id="f-keyname"
              className="input"
              placeholder="my-laptop"
              value={newName}
              onChange={(event) => setNewName(event.target.value)}
            />
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

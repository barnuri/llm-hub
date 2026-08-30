import { useState } from "react";

import { api } from "../lib/api";

interface FallbacksEditorProps {
  readonly chain: readonly string[];
  readonly models: readonly string[];
  readonly readonly: boolean;
  readonly onSaved: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

/**
 * Optional ordered default fallback chain: when a request's model fails, the
 * hub retries these models in order. Persisted as LLM_HUB_DEFAULT_FALLBACKS;
 * callers can still override per request with the x-llm-hub-fallbacks header.
 *
 * Mount with `key={chain.join(",")}` so a server-side change resets the draft.
 */
export function FallbacksEditor({ chain, models, readonly, onSaved, onToast }: FallbacksEditorProps) {
  const [draft, setDraft] = useState<readonly string[]>(chain);
  const [saving, setSaving] = useState(false);

  const dirty = draft.join(",") !== chain.join(",");
  const available = models.filter((id) => !draft.includes(id));

  const move = (index: number, delta: -1 | 1) => {
    const target = index + delta;
    if (target < 0 || target >= draft.length) {
      return;
    }
    const next = [...draft];
    [next[index], next[target]] = [next[target] as string, next[index] as string];
    setDraft(next);
  };

  const save = async () => {
    setSaving(true);
    try {
      await api("/api/fallbacks", { method: "POST", body: JSON.stringify({ fallbacks: draft }) });
      onToast("Default fallbacks saved");
      onSaved();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="panel">
      <div className="toolbar">
        <b>Default fallbacks</b>
        <span className="dim">
          optional — tried in order when the requested model fails; per-request override:{" "}
          <span className="mono">x-llm-hub-fallbacks</span>
        </span>
      </div>
      {draft.length === 0 ? (
        <p className="dim">No default fallbacks. Pick a model below to build a chain.</p>
      ) : (
        <ol className="chain-list">
          {draft.map((id, index) => (
            <li key={id}>
              <span className="mono">{id}</span>
              <span className="chain-actions">
                <button
                  type="button"
                  className="btn small"
                  aria-label={`Move ${id} up`}
                  disabled={readonly || index === 0}
                  onClick={() => move(index, -1)}
                >
                  ↑
                </button>{" "}
                <button
                  type="button"
                  className="btn small"
                  aria-label={`Move ${id} down`}
                  disabled={readonly || index === draft.length - 1}
                  onClick={() => move(index, 1)}
                >
                  ↓
                </button>{" "}
                <button
                  type="button"
                  className="btn small danger"
                  aria-label={`Remove ${id} from fallbacks`}
                  disabled={readonly}
                  onClick={() => setDraft(draft.filter((entry) => entry !== id))}
                >
                  ✕
                </button>
              </span>
            </li>
          ))}
        </ol>
      )}
      <div className="toolbar">
        <select
          className="input"
          aria-label="Add fallback model"
          value=""
          disabled={readonly || available.length === 0}
          onChange={(event) => {
            if (event.target.value) {
              setDraft([...draft, event.target.value]);
            }
          }}
        >
          <option value="">Add a model to the chain…</option>
          {available.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
        <button type="button" className="btn primary" onClick={save} disabled={readonly || !dirty || saving}>
          {saving ? "Saving…" : "Save fallbacks"}
        </button>
      </div>
    </div>
  );
}

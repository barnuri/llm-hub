import { useState } from "react";

import { api } from "../lib/api";
import { Panel } from "./Panel";
import { Pill } from "./Pill";
import { Select } from "./Select";

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
    <Panel title="Default fallbacks" sub="Tried in order when the requested model fails" className="fallbacks">
      {draft.length === 0 ? (
        <p className="note fallbacks-empty">No default fallbacks. Pick a model below to build a chain.</p>
      ) : (
        <div className="table-scroll">
          <table className="table">
            <thead>
              <tr>
                <th className="fallbacks-index">#</th>
                <th>Model</th>
                <th>
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {draft.map((id, index) => (
                <tr key={id}>
                  <td className="num dim fallbacks-index">{index + 1}</td>
                  <td className="mono">{id}</td>
                  <td className="fallbacks-actions">
                    <button
                      type="button"
                      className="btn small"
                      aria-label={`Move ${id} up`}
                      disabled={readonly || index === 0}
                      onClick={() => move(index, -1)}
                    >
                      Up
                    </button>
                    <button
                      type="button"
                      className="btn small"
                      aria-label={`Move ${id} down`}
                      disabled={readonly || index === draft.length - 1}
                      onClick={() => move(index, 1)}
                    >
                      Down
                    </button>
                    <button
                      type="button"
                      className="btn small danger"
                      aria-label={`Remove ${id} from fallbacks`}
                      disabled={readonly}
                      onClick={() => setDraft(draft.filter((entry) => entry !== id))}
                    >
                      Remove
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="toolbar fallbacks-add">
        <label className="dim" htmlFor="fallbacks-add">
          Add model
        </label>
        <Select
          id="fallbacks-add"
          value=""
          placeholder="Add a model to the chain…"
          disabled={readonly || available.length === 0}
          options={available.map((id) => ({ value: id, label: id }))}
          onChange={(next) => {
            if (next) {
              setDraft([...draft, next]);
            }
          }}
        />
        <button type="button" className="btn primary" onClick={save} disabled={readonly || !dirty || saving}>
          {saving ? "Saving…" : "Save fallbacks"}
        </button>
      </div>
      <div className="fallbacks-foot">
        <span>
          Per-request override header: <code>x-llm-hub-fallbacks</code>
        </span>
        {dirty ? <Pill>unsaved changes</Pill> : null}
      </div>
    </Panel>
  );
}

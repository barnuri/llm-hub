import { useState } from "react";

import { api } from "../lib/api";
import { Panel } from "./Panel";

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
const CHAIN_ICON_PATHS = {
  up: "M12 19V5M6 11l6-6 6 6",
  down: "M12 5v14M6 13l6 6 6-6",
  remove: "M6 6l12 12M18 6L6 18",
} as const;

function ChainIcon({ name }: { readonly name: keyof typeof CHAIN_ICON_PATHS }) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="14"
      height="14"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={CHAIN_ICON_PATHS[name]} />
    </svg>
  );
}

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
    <Panel title="Default fallbacks" sub="Optional. Tried in order when the requested model fails.">
      <p className="note">
        Per-request override: <code>x-llm-hub-fallbacks</code>
      </p>
      {draft.length === 0 ? (
        <p className="note">No default fallbacks. Pick a model below to build a chain.</p>
      ) : (
        <ol className="chain-list">
          {draft.map((id, index) => (
            <li key={id}>
              <span className="mono">{id}</span>
              <span className="chain-actions">
                <button
                  type="button"
                  className="btn small icon"
                  aria-label={`Move ${id} up`}
                  title="Move up"
                  disabled={readonly || index === 0}
                  onClick={() => move(index, -1)}
                >
                  <ChainIcon name="up" />
                </button>
                <button
                  type="button"
                  className="btn small icon"
                  aria-label={`Move ${id} down`}
                  title="Move down"
                  disabled={readonly || index === draft.length - 1}
                  onClick={() => move(index, 1)}
                >
                  <ChainIcon name="down" />
                </button>
                <button
                  type="button"
                  className="btn small icon danger"
                  aria-label={`Remove ${id} from fallbacks`}
                  title="Remove"
                  disabled={readonly}
                  onClick={() => setDraft(draft.filter((entry) => entry !== id))}
                >
                  <ChainIcon name="remove" />
                </button>
              </span>
            </li>
          ))}
        </ol>
      )}
      <div className="toolbar">
        <label className="dim" htmlFor="fallbacks-add">
          Add model
        </label>
        <select
          id="fallbacks-add"
          className="input"
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
    </Panel>
  );
}

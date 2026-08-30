import { useState } from "react";

import { savedKey } from "../lib/api";
import { SETUP_TARGETS, snippetFor } from "../lib/snippets";

interface SetupScreenProps {
  readonly models: readonly string[];
  readonly authEnabled: boolean;
  readonly onCopy: (text: string) => void;
}

export function SetupScreen({ models, authEnabled, onCopy }: SetupScreenProps) {
  const [target, setTarget] = useState(SETUP_TARGETS[0][0]);
  const [model, setModel] = useState<string>(models[0] ?? "<profile>/<model>");
  const [fallbacks, setFallbacks] = useState<readonly string[]>([]);

  const key = authEnabled ? (savedKey() ?? "<your-hub-key>") : "dummy";
  const snippet = snippetFor(target, window.location.origin, model, key, fallbacks);
  const available = models.filter((id) => id !== model && !fallbacks.includes(id));

  return (
    <section className="tab active">
      <div className="toolbar">
        <label className="dim" htmlFor="setup-target">
          Tool
        </label>
        <select id="setup-target" className="input" value={target} onChange={(event) => setTarget(event.target.value)}>
          {SETUP_TARGETS.map(([value, label]) => (
            <option key={value} value={value}>
              {label}
            </option>
          ))}
        </select>
        <label className="dim" htmlFor="setup-model">
          Model
        </label>
        <select id="setup-model" className="input" value={model} onChange={(event) => setModel(event.target.value)}>
          {models.map((id) => (
            <option key={id}>{id}</option>
          ))}
        </select>
        <label className="dim" htmlFor="setup-fallbacks">
          Fallbacks (optional)
        </label>
        <select
          id="setup-fallbacks"
          className="input"
          value=""
          disabled={available.length === 0}
          onChange={(event) => {
            if (event.target.value) {
              setFallbacks([...fallbacks, event.target.value]);
            }
          }}
        >
          <option value="">Add fallback model…</option>
          {available.map((id) => (
            <option key={id} value={id}>
              {id}
            </option>
          ))}
        </select>
      </div>
      {fallbacks.length > 0 ? (
        <div className="toolbar chips" role="group" aria-label="Selected fallback models">
          <span className="dim">tried in order:</span>
          {fallbacks.map((id) => (
            <button
              key={id}
              type="button"
              className="chip active"
              aria-label={`Remove ${id} from fallbacks`}
              title="Click to remove"
              onClick={() => setFallbacks(fallbacks.filter((entry) => entry !== id))}
            >
              {id} ✕
            </button>
          ))}
        </div>
      ) : null}
      <p className="note">
        {authEnabled
          ? "Auth is on — use your master key or a hub API key from the API keys tab."
          : "Auth is off — any placeholder key works (SDKs require one, so “dummy” is used)."}
      </p>
      <pre className="snippet">
        <code>{snippet}</code>
      </pre>
      <button type="button" className="btn" onClick={() => onCopy(snippet)}>
        Copy snippet
      </button>
    </section>
  );
}

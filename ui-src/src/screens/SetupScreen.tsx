import { useState } from "react";

import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { savedKey } from "../lib/api";
import { SETUP_TARGETS, snippetFor } from "../lib/snippets";
import "../styles/screens/setup.css";

interface SetupScreenProps {
  readonly models: readonly string[];
  readonly authEnabled: boolean;
  readonly onCopy: (text: string) => void;
}

function RemoveIcon() {
  return (
    <svg
      viewBox="0 0 16 16"
      width="12"
      height="12"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d="M4 4l8 8M12 4l-8 8" />
    </svg>
  );
}

export function SetupScreen({ models, authEnabled, onCopy }: SetupScreenProps) {
  const [target, setTarget] = useState(SETUP_TARGETS[0][0]);
  const [model, setModel] = useState<string>(models[0] ?? "<profile>/<model>");
  const [fallbacks, setFallbacks] = useState<readonly string[]>([]);

  const key = authEnabled ? (savedKey() ?? "<your-hub-key>") : "dummy";
  const snippet = snippetFor(target, window.location.origin, model, key, fallbacks);
  const available = models.filter((id) => id !== model && !fallbacks.includes(id));
  const targetLabel = SETUP_TARGETS.find(([value]) => value === target)?.[1] ?? target;

  return (
    <section className="tab active setup">
      <div className="setup-grid">
        <Panel title="Configuration">
          <div className="field">
            <label htmlFor="setup-target">Tool</label>
            <select id="setup-target" className="input" value={target} onChange={(event) => setTarget(event.target.value)}>
              {SETUP_TARGETS.map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor="setup-model">Model</label>
            <select id="setup-model" className="input" value={model} onChange={(event) => setModel(event.target.value)}>
              {models.map((id) => (
                <option key={id}>{id}</option>
              ))}
            </select>
          </div>
          <div className="field">
            <label htmlFor="setup-fallbacks">Fallbacks (optional)</label>
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
            {fallbacks.length > 0 ? (
              <div className="chips setup-fallbacks" role="group" aria-label="Selected fallback models">
                <span className="dim">Tried in order:</span>
                {fallbacks.map((id) => (
                  <button
                    key={id}
                    type="button"
                    className="chip active setup-chip"
                    aria-label={`Remove ${id} from fallbacks`}
                    title="Remove"
                    onClick={() => setFallbacks(fallbacks.filter((entry) => entry !== id))}
                  >
                    {id}
                    <RemoveIcon />
                  </button>
                ))}
              </div>
            ) : null}
          </div>
          <div className="setup-auth">
            <Pill tone={authEnabled ? "ok" : "neutral"} dot>
              {authEnabled ? "Auth on" : "Auth off"}
            </Pill>
            <p className="note">
              {authEnabled
                ? "Use your master key or a hub API key from the API keys tab."
                : "Any placeholder key works (SDKs require one, so “dummy” is used)."}
            </p>
          </div>
        </Panel>
        <Panel
          title="Snippet"
          sub={targetLabel}
          actions={
            <button type="button" className="btn primary" onClick={() => onCopy(snippet)}>
              Copy snippet
            </button>
          }
        >
          <pre className="snippet">
            <code>{snippet}</code>
          </pre>
        </Panel>
      </div>
    </section>
  );
}

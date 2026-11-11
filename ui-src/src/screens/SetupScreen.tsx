import { useState } from "react";

import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { Select } from "../components/Select";
import { savedKey } from "../lib/api";
import { formatNumber } from "../lib/format";
import { SETUP_TARGETS, snippetFor } from "../lib/snippets";
import "../styles/screens/setup.css";

interface SetupScreenProps {
  readonly models: readonly string[];
  readonly authEnabled: boolean;
  readonly onCopy: (text: string) => void;
}

const FALLBACKS_HEADER = "X-LLM-Hub-Fallbacks";
const COMMENT_LINE = /^\s*(#|\/\/)/;

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
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

interface SnippetCodeProps {
  readonly snippet: string;
  readonly highlights: readonly string[];
}

function SnippetCode({ snippet, highlights }: SnippetCodeProps) {
  const terms = [...new Set(highlights.filter((term) => term !== ""))].sort(
    (left, right) => right.length - left.length,
  );
  const pattern = terms.length > 0 ? new RegExp(`(${terms.map(escapeRegExp).join("|")})`) : null;
  const lines = snippet.split("\n");

  return (
    <pre className="snippet">
      <code>
        {lines.map((line, lineIndex) => {
          const end = lineIndex < lines.length - 1 ? "\n" : "";
          if (COMMENT_LINE.test(line)) {
            return (
              <span key={lineIndex} className="snippet-comment">
                {line + end}
              </span>
            );
          }
          const parts = pattern ? line.split(pattern) : [line];
          return (
            <span key={lineIndex}>
              {parts.map((part, partIndex) =>
                partIndex % 2 === 1 ? (
                  <span key={partIndex} className="snippet-value">
                    {part}
                  </span>
                ) : (
                  part
                ),
              )}
              {end}
            </span>
          );
        })}
      </code>
    </pre>
  );
}

export function SetupScreen({ models, authEnabled, onCopy }: SetupScreenProps) {
  const [target, setTarget] = useState(SETUP_TARGETS[0][0]);
  const [model, setModel] = useState<string>(models[0] ?? "<profile>/<model>");
  const [fallbacks, setFallbacks] = useState<readonly string[]>([]);

  const origin = window.location.origin;
  const key = authEnabled ? (savedKey() ?? "<your-hub-key>") : "dummy";
  const snippet = snippetFor(target, origin, model, key, fallbacks);
  const available = models.filter((id) => id !== model && !fallbacks.includes(id));
  const targetLabel = SETUP_TARGETS.find(([value]) => value === target)?.[1] ?? target;

  return (
    <section className="tab active setup">
      <div className="setup-grid">
        <Panel className="setup-config">
          <div className="setup-tools" role="group" aria-labelledby="setup-tools-title">
            <div id="setup-tools-title" className="setup-label">
              Tool
            </div>
            {SETUP_TARGETS.map(([value, label]) => (
              <button
                key={value}
                type="button"
                className="setup-tool"
                aria-pressed={value === target}
                onClick={() => setTarget(value)}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="field">
            <label htmlFor="setup-model">Model</label>
            <Select id="setup-model" value={model} options={models.map((id) => ({ value: id, label: id }))} onChange={setModel} />
          </div>
          <div className="field">
            <label htmlFor="setup-fallbacks">Fallbacks (optional), tried in order</label>
            {fallbacks.length > 0 ? (
              <div className="chips setup-fallbacks" role="group" aria-label="Selected fallback models">
                {fallbacks.map((id, index) => (
                  <button
                    key={id}
                    type="button"
                    className="chip active setup-chip"
                    aria-label={`Remove ${id} from fallbacks`}
                    title="Remove"
                    onClick={() => setFallbacks(fallbacks.filter((entry) => entry !== id))}
                  >
                    <span className="setup-chip-index">{index + 1}</span>
                    {id}
                    <RemoveIcon />
                  </button>
                ))}
              </div>
            ) : null}
            <Select
              id="setup-fallbacks"
              value=""
              placeholder="Add fallback model…"
              disabled={available.length === 0}
              options={available.map((id) => ({ value: id, label: id }))}
              onChange={(next) => {
                if (next) {
                  setFallbacks([...fallbacks, next]);
                }
              }}
            />
          </div>
          <div className="setup-auth">
            <Pill tone={authEnabled ? "ok" : "neutral"}>{authEnabled ? "Auth on" : "Auth off"}</Pill>
            <p className="note">
              {authEnabled
                ? "Use your master key or a hub API key from the API keys tab."
                : "Any placeholder key works (SDKs require one, so “dummy” is used). With auth on, use your master key or a hub API key from the API keys tab."}
            </p>
          </div>
        </Panel>
        <Panel
          title="Snippet"
          sub={targetLabel}
          className="setup-snippet"
          actions={
            <button type="button" className="btn primary" onClick={() => onCopy(snippet)}>
              Copy snippet
            </button>
          }
        >
          <SnippetCode snippet={snippet} highlights={[origin, key, model, fallbacks.join(",")]} />
          <dl className="setup-meta">
            <div>
              <dt>Base URL</dt>
              <dd className="mono">{origin}</dd>
            </div>
            <div>
              <dt>Models</dt>
              <dd className="mono">{formatNumber(models.length)} available</dd>
            </div>
            <div>
              <dt>Fallback header</dt>
              <dd className="mono">{FALLBACKS_HEADER}</dd>
            </div>
          </dl>
        </Panel>
      </div>
    </section>
  );
}

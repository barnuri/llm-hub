import { type FormEvent, useState } from "react";

import { api } from "../lib/api";
import type { KeepWarmSettings } from "../lib/types";
import { Panel } from "./Panel";

const MIN_IDLE_CAP_MINUTES = 5;

interface KeepWarmPanelProps {
  readonly settings: KeepWarmSettings;
  readonly readonly: boolean;
  readonly onSaved: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

/**
 * Turns the cache keep-warm pings on or off and sets how long a stream may stay idle.
 * Mount with `key` built from the saved values so a server-side change resets the draft.
 */
export function KeepWarmPanel({ settings, readonly, onSaved, onToast }: KeepWarmPanelProps) {
  const [enabled, setEnabled] = useState(settings.enabled);
  const [cap, setCap] = useState(String(settings.idle_cap_minutes));
  const [saving, setSaving] = useState(false);

  const parsed = Number(cap);
  const capValid = cap.trim() !== "" && Number.isInteger(parsed) && parsed >= MIN_IDLE_CAP_MINUTES;
  const locked = readonly || settings.overridden_by_env;
  const dirty = enabled !== settings.enabled || (capValid && parsed !== settings.idle_cap_minutes);

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setSaving(true);
    try {
      await api("/api/keep-warm", {
        method: "POST",
        body: JSON.stringify({ enabled, idle_cap_minutes: parsed }),
      });
      onToast(enabled ? "Keep-warm settings saved" : "Keep-warm turned off");
      onSaved();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Panel title="Cache keep-warm" sub="Copilot Claude prompt cache">
      <form className="keep-warm-form" onSubmit={(event) => void save(event)}>
        <label className="keep-warm-toggle">
          <input
            type="checkbox"
            checked={enabled}
            disabled={locked || saving}
            onChange={(event) => setEnabled(event.target.checked)}
          />
          <span>Keep idle streams warm</span>
        </label>
        <label className="keep-warm-field">
          <span>Stop after idle for</span>
          <input
            className="input keep-warm-cap"
            type="number"
            min={MIN_IDLE_CAP_MINUTES}
            step={1}
            inputMode="numeric"
            aria-label="Idle cap in minutes"
            aria-invalid={!capValid}
            value={cap}
            disabled={locked || saving}
            onChange={(event) => setCap(event.target.value)}
          />
          <span>minutes</span>
        </label>
        <button type="submit" className="btn small" disabled={locked || saving || !dirty || !capValid}>
          {saving ? "Saving…" : "Save"}
        </button>
      </form>
      <p className="dim models-side-note">
        Saved as <code>LLM_HUB_KEEP_WARM</code> and <code>LLM_HUB_KEEP_WARM_IDLE_CAP_MINUTES</code>. Pings go out at
        most 10 per minute in total.
        {readonly ? " Config is read-only." : ""}
      </p>
      {settings.overridden_by_env ? (
        <p className="keep-warm-notice" role="status">
          The service environment sets this value, so saved changes do not take effect until that variable is removed.
        </p>
      ) : null}
    </Panel>
  );
}

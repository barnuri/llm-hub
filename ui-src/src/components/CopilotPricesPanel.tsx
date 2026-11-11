import { type FormEvent, useState } from "react";

import { api } from "../lib/api";
import { NumberField } from "./NumberField";
import { Panel } from "./Panel";

const SECONDS_PER_HOUR = 3600;

interface CopilotPricesPanelProps {
  /** `LLM_HUB_COPILOT_PRICES_REFRESH_SECS`; 0 means the fetch is off. */
  readonly refreshSecs: number;
  readonly readonly: boolean;
  readonly onSaved: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

/**
 * Edits how often the hub fetches Copilot prices from GitHub Docs.
 * Mount with `key={refreshSecs}` so a server-side change resets the draft.
 */
export function CopilotPricesPanel({ refreshSecs, readonly, onSaved, onToast }: CopilotPricesPanelProps) {
  const [hours, setHours] = useState(String(refreshSecs / SECONDS_PER_HOUR));
  const [saving, setSaving] = useState(false);

  const parsed = Number(hours);
  const valid = hours.trim() !== "" && Number.isFinite(parsed) && parsed >= 0;
  const secs = valid ? Math.round(parsed * SECONDS_PER_HOUR) : refreshSecs;
  const dirty = valid && secs !== refreshSecs;

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setSaving(true);
    try {
      await api("/api/copilot/prices", { method: "POST", body: JSON.stringify({ refresh_secs: secs }) });
      onToast(secs === 0 ? "Copilot price fetch turned off" : "Copilot price refresh interval saved");
      onSaved();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Panel title="Copilot prices" sub="fetched from GitHub Docs">
      <form className="copilot-prices-form" onSubmit={(event) => void save(event)}>
        <label className="copilot-prices-field">
          <span>Refresh every</span>
          <NumberField
            className="copilot-prices-hours"
            min={0}
            inputMode="decimal"
            ariaLabel="Refresh interval in hours"
            invalid={!valid}
            value={hours}
            disabled={readonly || saving}
            onChange={setHours}
          />
          <span>hours</span>
        </label>
        <button type="submit" className="btn small" disabled={readonly || saving || !dirty}>
          {saving ? "Saving…" : "Save"}
        </button>
      </form>
      <p className="dim models-side-note">
        Saved as <code>LLM_HUB_COPILOT_PRICES_REFRESH_SECS</code>. 0 turns the fetch off. Fetches stay at least 1
        hour apart.
        {readonly ? " Config is read-only." : ""}
      </p>
    </Panel>
  );
}

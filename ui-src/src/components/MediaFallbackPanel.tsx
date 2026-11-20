import { type FormEvent, useState } from "react";

import { api } from "../lib/api";
import type { MediaFallbackSettings } from "../lib/types";
import { Checkbox } from "./Checkbox";
import { Panel } from "./Panel";
import { Select, type SelectOption } from "./Select";

const NONE = "";

interface MediaFallbackPanelProps {
  readonly settings: MediaFallbackSettings;
  /** Every model id the hub lists, offered as the vision and speech model. */
  readonly modelIds: readonly string[];
  readonly readonly: boolean;
  readonly onSaved: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

/** The options of one model picker: "off" first, then every listed model, plus the saved one if it is not listed. */
function modelOptions(modelIds: readonly string[], saved: string): SelectOption[] {
  const ids = saved !== NONE && !modelIds.includes(saved) ? [saved, ...modelIds] : modelIds;
  return [{ value: NONE, label: "Off" }, ...ids.map((id) => ({ value: id, label: id }))];
}

/** Mount with a `key` built from the saved values so a server-side change resets the draft. */
export function MediaFallbackPanel({ settings, modelIds, readonly, onSaved, onToast }: MediaFallbackPanelProps) {
  const savedVision = settings.vision_model ?? NONE;
  const savedStt = settings.stt_model ?? NONE;
  const [enabled, setEnabled] = useState(settings.enabled);
  const [skip, setSkip] = useState(settings.skip);
  const [vision, setVision] = useState(savedVision);
  const [stt, setStt] = useState(savedStt);
  const [saving, setSaving] = useState(false);

  const locked = readonly || settings.overridden_by_env;
  const dirty = enabled !== settings.enabled || skip !== settings.skip || vision !== savedVision || stt !== savedStt;

  const save = async (event: FormEvent) => {
    event.preventDefault();
    setSaving(true);
    try {
      await api("/api/media-fallback", {
        method: "POST",
        body: JSON.stringify({ enabled, skip, vision_model: vision, stt_model: stt }),
      });
      onToast(enabled || skip ? "Media fallback settings saved" : "Media fallback turned off");
      onSaved();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Panel title="Media fallback" sub="Text for text-only models">
      <form className="keep-warm-form" onSubmit={(event) => void save(event)}>
        <Checkbox
          checked={enabled}
          disabled={locked || saving}
          onChange={setEnabled}
          label="Turn images, audio and files into text"
        />
        <Checkbox
          checked={skip}
          disabled={locked || saving}
          onChange={setSkip}
          label="Skip media no model converts, and tell the model it was removed"
        />
        <label className="keep-warm-field">
          <span>Captions by</span>
          <Select
            ariaLabel="Vision model"
            value={vision}
            options={modelOptions(modelIds, savedVision)}
            disabled={locked || saving}
            onChange={setVision}
          />
        </label>
        <label className="keep-warm-field">
          <span>Transcripts by</span>
          <Select
            ariaLabel="Speech model"
            value={stt}
            options={modelOptions(modelIds, savedStt)}
            disabled={locked || saving}
            onChange={setStt}
          />
        </label>
        <button type="submit" className="btn small" disabled={locked || saving || !dirty}>
          {saving ? "Saving…" : "Save"}
        </button>
      </form>
      <p className="dim models-side-note">
        Saved as <code>LLM_HUB_MEDIA_FALLBACK</code>, <code>LLM_HUB_MEDIA_SKIP</code>,{" "}
        <code>LLM_HUB_MEDIA_FALLBACK_VISION_MODEL</code> and <code>LLM_HUB_MEDIA_FALLBACK_STT_MODEL</code>. The caption
        and transcript models get only the image or audio, never the chat. Results are cached, so a resent history is
        not converted again. Skip drops the part with no model call and leaves a system reminder in its place.
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

import { type FormEvent, useEffect, useRef, useState } from "react";

import { api } from "../lib/api";
import type { ProfileRow } from "../lib/types";
import { Checkbox } from "./Checkbox";
import { Panel } from "./Panel";
import { Pill } from "./Pill";

interface ProfileFormPanelProps {
  /** The profile being edited, or null to add a new one. Mount with a key per target so the draft resets. */
  readonly existing: ProfileRow | null;
  readonly onSaved: () => void;
  readonly onCancel: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

interface FormErrors {
  readonly name?: string;
  readonly baseUrl?: string;
}

const HTTP_URL = /^https?:\/\//;

function validate(name: string, baseUrl: string): FormErrors {
  return {
    name: name === "" || name.includes("/") ? "Enter a name without a “/”." : undefined,
    baseUrl: baseUrl !== "" && !HTTP_URL.test(baseUrl) ? "Start the URL with http:// or https://." : undefined,
  };
}

function FieldError({ id, error }: { readonly id: string; readonly error: string | undefined }) {
  if (!error) {
    return null;
  }
  return (
    <p id={`${id}-error`} className="field-error" role="alert">
      {error}
    </p>
  );
}

export function ProfileFormPanel({ existing, onSaved, onCancel, onToast }: ProfileFormPanelProps) {
  const [name, setName] = useState(existing?.name ?? "");
  const [displayName, setDisplayName] = useState(existing?.display_name ?? "");
  const [baseUrl, setBaseUrl] = useState(existing?.base_url ?? "");
  const [apiKey, setApiKey] = useState("");
  const [staticModels, setStaticModels] = useState(existing?.models.join(",") ?? "");
  const [enabled, setEnabled] = useState(existing?.enabled ?? true);
  const [keepWarm, setKeepWarm] = useState(existing?.keep_warm ?? true);
  const [errors, setErrors] = useState<FormErrors>({});
  const [saving, setSaving] = useState(false);
  const firstField = useRef<HTMLInputElement>(null);

  useEffect(() => {
    firstField.current?.focus();
  }, []);

  const fieldProps = (id: string, error: string | undefined) => ({
    id,
    className: "input",
    "aria-invalid": error !== undefined,
    "aria-describedby": error ? `${id}-error` : undefined,
  });

  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const found = validate(name.trim(), baseUrl.trim());
    if (found.name || found.baseUrl) {
      setErrors(found);
      return;
    }
    setSaving(true);
    try {
      await api("/api/profiles", {
        method: "POST",
        body: JSON.stringify({
          name: name.trim(),
          display_name: displayName.trim() || null,
          base_url: baseUrl.trim(),
          api_key: apiKey || undefined,
          models: staticModels.split(",").map((s) => s.trim()).filter(Boolean),
          enabled,
          keep_warm: keepWarm,
        }),
      });
      onToast(existing ? "Profile saved" : "Profile added");
      onSaved();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    } finally {
      setSaving(false);
    }
  };

  const title = existing ? `Edit profile ${existing.name}` : "Add profile";
  const keyPlaceholder = existing?.api_key_masked ? `current: ${existing.api_key_masked}` : undefined;

  return (
    <Panel
      title={title}
      className="profile-form"
      actions={
        <Pill tone={enabled ? "ok" : "neutral"}>{enabled ? "Enabled" : "Disabled"}</Pill>
      }
    >
      <form onSubmit={save} noValidate aria-label={title}>
        <div className="field">
          <label htmlFor="f-name">Name (becomes the model prefix)</label>
          <input
            {...fieldProps("f-name", errors.name)}
            ref={existing ? undefined : firstField}
            value={name}
            readOnly={existing !== null}
            onChange={(event) => {
              setName(event.target.value);
              setErrors({ ...errors, name: undefined });
            }}
          />
          <FieldError id="f-name" error={errors.name} />
        </div>
        <div className="field">
          <label htmlFor="f-display">Display name (optional UI label)</label>
          <input
            id="f-display"
            ref={existing ? firstField : undefined}
            className="input"
            placeholder={name.trim() || "same as name"}
            value={displayName}
            onChange={(event) => setDisplayName(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="f-url">Base URL (OpenAI-compatible, ends with /v1)</label>
          <input
            {...fieldProps("f-url", errors.baseUrl)}
            placeholder="https://api.groq.com/openai/v1"
            value={baseUrl}
            onChange={(event) => {
              setBaseUrl(event.target.value);
              setErrors({ ...errors, baseUrl: undefined });
            }}
          />
          <FieldError id="f-url" error={errors.baseUrl} />
        </div>
        <div className="field">
          <label htmlFor="f-apikey">API key {existing ? "(leave empty to keep current)" : ""}</label>
          <input
            id="f-apikey"
            className="input"
            type="password"
            autoComplete="off"
            placeholder={keyPlaceholder}
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="f-models">Static models (comma list, only if upstream has no /v1/models)</label>
          <input
            id="f-models"
            className="input"
            placeholder="empty, discovered from /v1/models"
            value={staticModels}
            onChange={(event) => setStaticModels(event.target.value)}
          />
        </div>
        <div className="profile-form-foot">
          <div className="profile-form-checks">
            <Checkbox checked={enabled} onChange={setEnabled} label="Enabled" />
            <Checkbox checked={keepWarm} onChange={setKeepWarm} label="Keep prompt cache warm" />
          </div>
          <div className="profile-form-buttons">
            <button type="button" className="btn small" onClick={onCancel}>
              Cancel
            </button>
            <button type="submit" className="btn small primary" disabled={saving}>
              {saving ? "Saving…" : existing ? "Save changes" : "Add profile"}
            </button>
          </div>
        </div>
      </form>
    </Panel>
  );
}

import { useState } from "react";

import { FallbacksEditor } from "../components/FallbacksEditor";
import { Modal } from "../components/Modal";
import { Panel } from "../components/Panel";
import { Pill } from "../components/Pill";
import { api } from "../lib/api";
import type { HubMeta, ProfileRow } from "../lib/types";
import "../styles/screens/profiles.css";

interface ProfilesScreenProps {
  readonly meta: HubMeta | null;
  readonly models: readonly string[];
  readonly onChanged: () => void;
  readonly onToast: (message: string, isError?: boolean) => void;
}

interface FormState {
  readonly existing: ProfileRow | null;
  name: string;
  displayName: string;
  baseUrl: string;
  apiKey: string;
  models: string;
  enabled: boolean;
}

interface FormErrors {
  readonly name?: string;
  readonly baseUrl?: string;
}

const HTTP_URL = /^https?:\/\//;

function validate(form: FormState): FormErrors {
  const name = form.name.trim();
  const baseUrl = form.baseUrl.trim();
  return {
    name: name === "" || name.includes("/") ? "Enter a name without a “/”." : undefined,
    baseUrl: baseUrl !== "" && !HTTP_URL.test(baseUrl) ? "Start the URL with http:// or https://." : undefined,
  };
}

function healthPill(profile: ProfileRow) {
  if (!profile.enabled) {
    return <Pill dot>Disabled</Pill>;
  }
  if (profile.healthy === false) {
    return (
      <Pill tone="danger" dot>
        Unreachable
      </Pill>
    );
  }
  return (
    <Pill tone="ok" dot>
      Enabled
    </Pill>
  );
}

export function ProfilesScreen({ meta, models, onChanged, onToast }: ProfilesScreenProps) {
  const [form, setForm] = useState<FormState | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [errors, setErrors] = useState<FormErrors>({});
  const readonly = meta?.readonly ?? false;

  const closeForm = () => {
    setForm(null);
    setErrors({});
  };

  const openAdd = () =>
    setForm({
      existing: null,
      name: "",
      displayName: "",
      baseUrl: "",
      apiKey: "",
      models: "",
      enabled: true,
    });

  const openEdit = (profile: ProfileRow) =>
    setForm({
      existing: profile,
      name: profile.name,
      displayName: profile.display_name ?? "",
      baseUrl: profile.base_url,
      apiKey: "",
      models: profile.models.join(","),
      enabled: profile.enabled,
    });

  const save = async () => {
    if (!form) {
      return;
    }
    const found = validate(form);
    if (found.name || found.baseUrl) {
      setErrors(found);
      return;
    }
    try {
      await api("/api/profiles", {
        method: "POST",
        body: JSON.stringify({
          name: form.name.trim(),
          display_name: form.displayName.trim() || null,
          base_url: form.baseUrl.trim(),
          api_key: form.apiKey || undefined,
          models: form.models.split(",").map((s) => s.trim()).filter(Boolean),
          enabled: form.enabled,
        }),
      });
      onToast(form.existing ? "Profile saved" : "Profile added");
      closeForm();
      onChanged();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    }
  };

  const remove = async () => {
    if (!deleting) {
      return;
    }
    try {
      await api(`/api/profiles/${encodeURIComponent(deleting)}`, { method: "DELETE" });
      onToast("Profile deleted");
      setDeleting(null);
      onChanged();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    }
  };

  const test = async (name: string) => {
    try {
      const result = await api<{ ok: boolean; latency_ms?: number; status?: number; error?: string }>(
        `/api/profiles/${encodeURIComponent(name)}/test`,
        { method: "POST" },
      );
      onToast(
        result.ok
          ? `${name}: reachable (${result.latency_ms} ms)`
          : `${name}: ${result.error ?? `status ${result.status}`}`,
        !result.ok,
      );
      onChanged();
    } catch (err) {
      onToast(err instanceof Error ? err.message : String(err), true);
    }
  };

  const profiles = meta?.profiles ?? [];

  const fieldProps = (id: string, error: string | undefined) => ({
    id,
    className: "input",
    "aria-invalid": error !== undefined,
    "aria-describedby": error ? `${id}-error` : undefined,
  });

  const fieldError = (id: string, error: string | undefined) =>
    error ? (
      <p id={`${id}-error`} className="field-error" role="alert">
        {error}
      </p>
    ) : null;

  return (
    <section className="tab active profiles">
      <Panel
        title="Upstream profiles"
        sub={meta ? `${profiles.length} configured` : undefined}
        actions={
          <>
            {readonly ? (
              <Pill tone="warn" dot title="Edits are disabled">
                Read-only
              </Pill>
            ) : null}
            <button type="button" className="btn primary" onClick={openAdd} disabled={readonly}>
              Add profile
            </button>
          </>
        }
      >
        {meta === null ? (
          <div className="skeleton profiles-skeleton" aria-label="Loading profiles" />
        ) : profiles.length === 0 ? (
          <p className="note profiles-empty">No profiles yet. Add one to route models through an upstream.</p>
        ) : (
          <div className="table-scroll profiles-scroll">
            <table className="table">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Label</th>
                  <th>Base URL</th>
                  <th>Key</th>
                  <th>Status</th>
                  <th>
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {profiles.map((profile) => (
                  <tr key={profile.name}>
                    <td className="mono">{profile.name}</td>
                    <td>{profile.label}</td>
                    <td className="mono dim profiles-url" title={profile.base_url}>
                      {profile.base_url}
                    </td>
                    <td className="mono dim">{profile.api_key_masked || "—"}</td>
                    <td>{healthPill(profile)}</td>
                    <td className="profiles-actions">
                      <button
                        type="button"
                        className="btn small"
                        aria-label={`Test ${profile.name}`}
                        onClick={() => test(profile.name)}
                      >
                        Test
                      </button>
                      <button
                        type="button"
                        className="btn small"
                        aria-label={`Edit ${profile.name}`}
                        onClick={() => openEdit(profile)}
                        disabled={readonly}
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        className="btn small danger"
                        aria-label={`Delete ${profile.name}`}
                        onClick={() => setDeleting(profile.name)}
                        disabled={readonly}
                      >
                        Delete
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      <FallbacksEditor
        key={(meta?.default_fallbacks ?? []).join(",")}
        chain={meta?.default_fallbacks ?? []}
        models={models}
        readonly={readonly}
        onSaved={onChanged}
        onToast={onToast}
      />

      {form ? (
        <Modal
          title={form.existing ? `Edit profile ${form.existing.name}` : "Add profile"}
          confirmLabel={form.existing ? "Save changes" : "Add profile"}
          onConfirm={save}
          onCancel={closeForm}
        >
          <div className="field">
            <label htmlFor="f-name">Name (becomes the model prefix)</label>
            <input
              {...fieldProps("f-name", errors.name)}
              value={form.name}
              readOnly={form.existing !== null}
              onChange={(event) => {
                setForm({ ...form, name: event.target.value });
                setErrors({ ...errors, name: undefined });
              }}
            />
            {fieldError("f-name", errors.name)}
          </div>
          <div className="field">
            <label htmlFor="f-display">Display name (optional UI label)</label>
            <input
              id="f-display"
              className="input"
              placeholder={form.name.trim() || "same as name"}
              value={form.displayName}
              onChange={(event) => setForm({ ...form, displayName: event.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="f-url">Base URL (OpenAI-compatible, ends with /v1)</label>
            <input
              {...fieldProps("f-url", errors.baseUrl)}
              placeholder="https://api.groq.com/openai/v1"
              value={form.baseUrl}
              onChange={(event) => {
                setForm({ ...form, baseUrl: event.target.value });
                setErrors({ ...errors, baseUrl: undefined });
              }}
            />
            {fieldError("f-url", errors.baseUrl)}
          </div>
          <div className="field">
            <label htmlFor="f-apikey">API key {form.existing ? "(leave empty to keep current)" : ""}</label>
            <input
              id="f-apikey"
              className="input"
              type="password"
              autoComplete="off"
              value={form.apiKey}
              onChange={(event) => setForm({ ...form, apiKey: event.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor="f-models">Static models (comma list, only if upstream has no /v1/models)</label>
            <input
              id="f-models"
              className="input"
              value={form.models}
              onChange={(event) => setForm({ ...form, models: event.target.value })}
            />
          </div>
          <div className="field">
            <label className="field-check">
              <input
                type="checkbox"
                checked={form.enabled}
                onChange={(event) => setForm({ ...form, enabled: event.target.checked })}
              />
              Enabled
            </label>
          </div>
        </Modal>
      ) : null}

      {deleting ? (
        <Modal title={`Delete profile ${deleting}`} confirmLabel="Delete profile" danger onConfirm={remove} onCancel={() => setDeleting(null)}>
          <p>
            Removes <b className="mono">{deleting}</b> and its env vars from .env. Models under{" "}
            <span className="mono">{deleting}/</span> stop resolving immediately.
          </p>
        </Modal>
      ) : null}
    </section>
  );
}

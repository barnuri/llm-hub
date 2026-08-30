import { useState } from "react";

import { Modal } from "../components/Modal";
import { api } from "../lib/api";
import type { HubMeta, ProfileRow } from "../lib/types";

interface ProfilesScreenProps {
  readonly meta: HubMeta | null;
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

export function ProfilesScreen({ meta, onChanged, onToast }: ProfilesScreenProps) {
  const [form, setForm] = useState<FormState | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const readonly = meta?.readonly ?? false;

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
      setForm(null);
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

  return (
    <section className="tab active">
      <div className="toolbar">
        <button type="button" className="btn primary" onClick={openAdd} disabled={readonly}>
          Add profile
        </button>
        {readonly ? <span className="dim">read-only mode — edits disabled</span> : null}
      </div>
      <table className="table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Label</th>
            <th>Base URL</th>
            <th>Key</th>
            <th>Enabled</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {(meta?.profiles ?? []).map((profile) => (
            <tr key={profile.name}>
              <td className="mono">{profile.name}</td>
              <td>{profile.label}</td>
              <td className="mono">{profile.base_url}</td>
              <td className="mono">{profile.api_key_masked || "—"}</td>
              <td>{profile.enabled ? "yes" : "no"}</td>
              <td>
                <button type="button" className="btn small" onClick={() => test(profile.name)}>
                  Test
                </button>{" "}
                <button type="button" className="btn small" onClick={() => openEdit(profile)} disabled={readonly}>
                  Edit
                </button>{" "}
                <button type="button" className="btn small danger" onClick={() => setDeleting(profile.name)} disabled={readonly}>
                  Delete
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {form ? (
        <Modal
          title={form.existing ? `Edit profile ${form.existing.name}` : "Add profile"}
          confirmLabel={form.existing ? "Save changes" : "Add profile"}
          onConfirm={save}
          onCancel={() => setForm(null)}
        >
          <div className="field">
            <label htmlFor="f-name">Name (becomes the model prefix)</label>
            <input
              id="f-name"
              className="input"
              value={form.name}
              readOnly={form.existing !== null}
              onChange={(event) => setForm({ ...form, name: event.target.value })}
            />
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
              id="f-url"
              className="input"
              placeholder="https://api.groq.com/openai/v1"
              value={form.baseUrl}
              onChange={(event) => setForm({ ...form, baseUrl: event.target.value })}
            />
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
            <label>
              <input
                type="checkbox"
                checked={form.enabled}
                onChange={(event) => setForm({ ...form, enabled: event.target.checked })}
              />{" "}
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

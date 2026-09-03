import { useCallback, useEffect, useRef, useState } from "react";

import { Modal } from "./components/Modal";
import { Toasts, type ToastItem } from "./components/Toasts";
import { api, saveKey, UnauthorizedError } from "./lib/api";
import { navigate, useRoute } from "./lib/router";
import type { HubMeta } from "./lib/types";
import { KeysScreen } from "./screens/KeysScreen";
import { ModelsScreen } from "./screens/ModelsScreen";
import { ProfilesScreen } from "./screens/ProfilesScreen";
import { SetupScreen } from "./screens/SetupScreen";
import { StatsScreen } from "./screens/StatsScreen";
import { ErrorsScreen } from "./screens/ErrorsScreen";
import { UsageScreen } from "./screens/UsageScreen";

const TABS: ReadonlyArray<readonly [string, string]> = [
  ["overview", "Overview"],
  ["errors", "Errors"],
  ["models", "Models"],
  ["stats", "Stats"],
  ["profiles", "Profiles"],
  ["keys", "API keys"],
  ["setup", "Setup"],
];

interface ModelsResponse {
  readonly data: ReadonlyArray<{ readonly id: string }>;
}

export function App() {
  const route = useRoute();
  const routeTab = route.path.split("/")[0] ?? "";
  const normalizedTab = routeTab === "usage" ? "overview" : routeTab;
  const tab = TABS.some(([id]) => id === normalizedTab) ? normalizedTab : "overview";
  const [meta, setMeta] = useState<HubMeta | null>(null);
  const [models, setModels] = useState<readonly string[]>([]);
  const [toasts, setToasts] = useState<readonly ToastItem[]>([]);
  const [askKey, setAskKey] = useState(false);
  const [confirmRestart, setConfirmRestart] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const keyInput = useRef<HTMLInputElement>(null);
  const nextToastId = useRef(1);

  const toast = useCallback((message: string, isError = false) => {
    const id = nextToastId.current;
    nextToastId.current += 1;
    setToasts((current) => [...current, { id, message, isError }]);
    setTimeout(() => setToasts((current) => current.filter((t) => t.id !== id)), 4000);
  }, []);

  const handleError = useCallback(
    (err: unknown) => {
      if (err instanceof UnauthorizedError) {
        setAskKey(true);
        return;
      }
      toast(err instanceof Error ? err.message : String(err), true);
    },
    [toast],
  );

  const loadAll = useCallback(() => {
    api<HubMeta>("/api/profiles").then(setMeta).catch(handleError);
    api<ModelsResponse>("/v1/models")
      .then((body) => setModels(body.data.map((m) => m.id).sort()))
      .catch(handleError);
  }, [handleError]);

  useEffect(loadAll, [loadAll]);

  const copy = useCallback(
    (text: string) => {
      navigator.clipboard
        .writeText(text)
        .then(() => toast(`Copied ${text.length > 60 ? "snippet" : text}`))
        .catch(() => toast("Clipboard unavailable", true));
    },
    [toast],
  );

  const restart = async () => {
    setConfirmRestart(false);
    try {
      await api("/api/restart", { method: "POST" });
    } catch (err) {
      handleError(err);
      return;
    }
    setRestarting(true);
    const deadline = Date.now() + 60_000;
    // Give the old process time to drain, then poll until the hub answers again.
    await new Promise((resolve) => setTimeout(resolve, 1500));
    while (Date.now() < deadline) {
      try {
        const response = await fetch("/healthz", { cache: "no-store" });
        if (response.ok) {
          window.location.reload();
          return;
        }
      } catch {
        // still down — keep polling
      }
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    setRestarting(false);
    toast("Hub did not come back within 60s — check the server logs", true);
  };

  const submitKey = () => {
    const value = keyInput.current?.value.trim() ?? "";
    if (value) {
      saveKey(value);
      setAskKey(false);
      loadAll();
    }
  };

  return (
    <>
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark">◆</span> llm-hub{" "}
          <span className="version">{meta ? `v${meta.version}` : ""}</span>
        </div>
        <div className="topbar-right">
          {meta ? (
            <>
              <span className={meta.auth_enabled ? "badge on" : "badge"}>
                {meta.auth_enabled ? "auth on" : "auth off"}
              </span>
              <span className={meta.persistent ? "badge on" : "badge"}>
                {meta.persistent ? "persistent" : "in-memory"}
              </span>
              <button type="button" className="btn small danger" onClick={() => setConfirmRestart(true)}>
                Restart
              </button>
            </>
          ) : null}
        </div>
      </header>

      <div className="shell">
        <nav className="sidebar">
          <div className="nav-tabs" role="tablist">
            {TABS.map(([id, label]) => (
              <button
                key={id}
                type="button"
                role="tab"
                className={tab === id ? "nav-tab active" : "nav-tab"}
                onClick={() => navigate(`/${id}`)}
              >
                {label}
              </button>
            ))}
          </div>
          <div className="ports">
            <div className="ports-title">Upstreams</div>
            <ul className="port-list">
              {(meta?.profiles ?? []).map((profile) => {
                const led = profile.healthy === true ? "up" : profile.healthy === false ? "down" : "off";
                return (
                  <li key={profile.name}>
                    <span className={`led ${led}`}></span>
                    {profile.label}
                  </li>
                );
              })}
            </ul>
          </div>
        </nav>

        <main className="content">
          {tab === "models" ? (
            <ModelsScreen models={models} profiles={meta?.profiles ?? []} onCopy={copy} />
          ) : null}
          {tab === "stats" ? <StatsScreen onError={(message) => toast(message, true)} /> : null}
          {tab === "overview" ? <UsageScreen /> : null}
          {tab === "errors" ? <ErrorsScreen /> : null}
          {tab === "profiles" ? (
            <ProfilesScreen meta={meta} models={models} onChanged={loadAll} onToast={toast} />
          ) : null}
          {tab === "keys" ? <KeysScreen onToast={toast} /> : null}
          {tab === "setup" ? (
            <SetupScreen models={models} authEnabled={meta?.auth_enabled ?? false} onCopy={copy} />
          ) : null}
        </main>
      </div>

      {confirmRestart ? (
        <Modal
          title="Restart llm-hub"
          confirmLabel="Restart now"
          danger
          onConfirm={restart}
          onCancel={() => setConfirmRestart(false)}
        >
          <p>
            In-flight requests are drained, then the server restarts. Supervised installs (
            <span className="mono">llm-hub service install</span>) come back via the supervisor; standalone
            runs respawn themselves. Expect a few seconds of downtime.
          </p>
        </Modal>
      ) : null}

      {restarting ? (
        <div className="restart-overlay" role="status">
          <div className="spinner" />
          <div>Restarting llm-hub…</div>
          <div className="dim">Reconnecting automatically</div>
        </div>
      ) : null}

      {askKey ? (
        <Modal title="API key required" confirmLabel="Save key" onConfirm={submitKey} onCancel={() => setAskKey(false)}>
          <div className="field">
            <label htmlFor="f-key">Master key or hub API key</label>
            <input id="f-key" ref={keyInput} className="input" type="password" autoComplete="off" />
          </div>
        </Modal>
      ) : null}

      <Toasts items={toasts} />
    </>
  );
}

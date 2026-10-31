import { useCallback, useEffect, useRef, useState } from "react";

import { Icon, type IconName } from "./components/Icon";
import { Modal } from "./components/Modal";
import { Pill } from "./components/Pill";
import { Segmented, type SegmentedOption } from "./components/Segmented";
import { ThemeToggle } from "./components/ThemeToggle";
import { Toasts, type ToastItem } from "./components/Toasts";
import { api, saveKey, UnauthorizedError } from "./lib/api";
import { navigate, setQueryParam, useRoute } from "./lib/router";
import type { HubMeta } from "./lib/types";
import { KeysScreen } from "./screens/KeysScreen";
import { ModelsScreen } from "./screens/ModelsScreen";
import { ProfilesScreen } from "./screens/ProfilesScreen";
import { SetupScreen } from "./screens/SetupScreen";
import { StatsScreen } from "./screens/StatsScreen";
import { ErrorsScreen } from "./screens/ErrorsScreen";
import { InsightsScreen } from "./screens/InsightsScreen";
import { AutoScreen } from "./screens/AutoScreen";
import { UsageScreen } from "./screens/UsageScreen";

interface Page {
  readonly id: string;
  readonly label: string;
  readonly icon: IconName;
  readonly periods?: readonly SegmentedOption<string>[];
}

interface NavGroup {
  readonly title: string;
  readonly pages: readonly Page[];
}

const OVERVIEW_PERIODS: readonly SegmentedOption<string>[] = [
  { value: "1d", label: "Today" },
  { value: "7d", label: "7 days" },
];

const NAV_GROUPS: readonly NavGroup[] = [
  {
    title: "Monitor",
    pages: [
      { id: "overview", label: "Overview", icon: "overview", periods: OVERVIEW_PERIODS },
      { id: "insights", label: "Insights", icon: "insights" },
      { id: "errors", label: "Errors", icon: "errors" },
      { id: "stats", label: "Stats", icon: "stats" },
      { id: "auto", label: "Copilot Auto", icon: "auto" },
    ],
  },
  {
    title: "Configure",
    pages: [
      { id: "models", label: "Models", icon: "models" },
      { id: "profiles", label: "Profiles", icon: "profiles" },
      { id: "keys", label: "API keys", icon: "keys" },
      { id: "setup", label: "Setup", icon: "setup" },
    ],
  },
];

const PAGES: readonly Page[] = NAV_GROUPS.flatMap((group) => group.pages);
const DEFAULT_PAGE: Page = PAGES[0] ?? { id: "overview", label: "Overview", icon: "overview" };

function periodOf(page: Page, requested: string | null): string | null {
  const periods = page.periods ?? [];
  const match = periods.find((option) => option.value === requested);
  return (match ?? periods[0])?.value ?? null;
}

interface ModelsResponse {
  readonly data: ReadonlyArray<{ readonly id: string }>;
}

export function App() {
  const route = useRoute();
  const routeTab = route.path.split("/")[0] ?? "";
  const normalizedTab = routeTab === "usage" ? "overview" : routeTab;
  const page = PAGES.find((candidate) => candidate.id === normalizedTab) ?? DEFAULT_PAGE;
  const tab = page.id;
  const period = periodOf(page, route.query.get("range"));
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
      <div className="app">
        <nav className="side" aria-label="Main">
          <div className="side-brand">
            <span className="brand-mark">
              <Icon name="logo" />
            </span>
            llm-hub
            <span className="version">{meta ? `v${meta.version}` : ""}</span>
          </div>
          {NAV_GROUPS.map((group) => (
            <div key={group.title} className="nav-group">
              <div className="nav-group-title">{group.title}</div>
              {group.pages.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className={tab === item.id ? "nav-item active" : "nav-item"}
                  aria-current={tab === item.id ? "page" : undefined}
                  onClick={() => navigate(`/${item.id}`)}
                >
                  <Icon name={item.icon} className="nav-icon" />
                  {item.label}
                </button>
              ))}
            </div>
          ))}
          <div className="nav-group side-upstreams">
            <div className="nav-group-title">Upstreams</div>
            <ul className="upstream-list">
              {(meta?.profiles ?? []).map((profile) => {
                const led = profile.healthy === true ? "up" : profile.healthy === false ? "down" : "off";
                const health = led === "up" ? "healthy" : led === "down" ? "unhealthy" : "unknown";
                return (
                  <li key={profile.name} className="upstream" title={`${profile.label}: ${health}`}>
                    <span className={`led ${led}`} aria-hidden="true"></span>
                    {profile.label}
                    <span className="sr-only">({health})</span>
                  </li>
                );
              })}
            </ul>
            {meta && meta.profiles.length === 0 ? <div className="upstream-empty">No profiles yet</div> : null}
          </div>
        </nav>

        <div className="main">
          <header className="header">
            <h1 className="page-title">{page.label}</h1>
            <div className="header-actions">
              {page.periods && period ? (
                <Segmented
                  label="Period"
                  options={page.periods}
                  value={period}
                  onChange={(value) => setQueryParam("range", value)}
                />
              ) : null}
              {meta ? (
                <>
                  <Pill tone={meta.auth_enabled ? "ok" : "neutral"} dot>
                    {meta.auth_enabled ? "auth on" : "auth off"}
                  </Pill>
                  <Pill tone={meta.persistent ? "ok" : "warn"} dot>
                    {meta.persistent ? "persistent" : "in-memory"}
                  </Pill>
                  <button type="button" className="btn small danger" onClick={() => setConfirmRestart(true)}>
                    <Icon name="restart" />
                    Restart
                  </button>
                </>
              ) : null}
              <ThemeToggle />
            </div>
          </header>

          <main className="content">
            {tab === "models" ? (
              <ModelsScreen models={models} profiles={meta?.profiles ?? []} onCopy={copy} />
            ) : null}
            {tab === "stats" ? <StatsScreen onError={(message) => toast(message, true)} /> : null}
            {tab === "overview" ? <UsageScreen period={period === "7d" ? "7d" : "1d"} /> : null}
            {tab === "errors" ? <ErrorsScreen /> : null}
            {tab === "auto" ? <AutoScreen /> : null}
            {tab === "insights" ? <InsightsScreen onCopy={copy} /> : null}
            {tab === "profiles" ? (
              <ProfilesScreen meta={meta} models={models} onChanged={loadAll} onToast={toast} />
            ) : null}
            {tab === "keys" ? <KeysScreen onToast={toast} /> : null}
            {tab === "setup" ? (
              <SetupScreen models={models} authEnabled={meta?.auth_enabled ?? false} onCopy={copy} />
            ) : null}
          </main>
        </div>
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

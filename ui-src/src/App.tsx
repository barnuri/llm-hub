import { useCallback, useEffect, useRef, useState } from "react";

import { Icon } from "./components/Icon";
import { Modal } from "./components/Modal";
import { Pill } from "./components/Pill";
import { ThemeToggle } from "./components/ThemeToggle";
import { Toasts, type ToastItem } from "./components/Toasts";
import { api, saveKey, UnauthorizedError } from "./lib/api";
import { formatNumber } from "./lib/format";
import { pricesById } from "./lib/pricing";
import { navigate, useRoute } from "./lib/router";
import type { ErrorsReport, HubMeta, HubModel } from "./lib/types";
import { KeysScreen } from "./screens/KeysScreen";
import { ModelsScreen } from "./screens/ModelsScreen";
import { ProfilesScreen } from "./screens/ProfilesScreen";
import { SetupScreen } from "./screens/SetupScreen";
import { StatsScreen } from "./screens/StatsScreen";
import { ErrorsScreen } from "./screens/ErrorsScreen";
import { InsightsScreen } from "./screens/InsightsScreen";
import { SessionsScreen } from "./screens/SessionsScreen";
import { AutoScreen } from "./screens/AutoScreen";
import { SmartAutoScreen } from "./screens/SmartAutoScreen";
import { type OverviewPeriod, UsageScreen } from "./screens/UsageScreen";

interface Page {
  readonly id: string;
  readonly label: string;
}

interface NavTab {
  readonly label: string;
  readonly pages: readonly Page[];
}

const SETTINGS_PAGES: readonly Page[] = [
  { id: "keys", label: "API keys" },
  { id: "setup", label: "Setup" },
];

const INSIGHTS_PAGES: readonly Page[] = [
  { id: "insights", label: "Insights" },
  { id: "sessions", label: "Sessions" },
];

const NAV_TABS: readonly NavTab[] = [
  { label: "Overview", pages: [{ id: "overview", label: "Overview" }] },
  { label: "Insights", pages: INSIGHTS_PAGES },
  { label: "Errors", pages: [{ id: "errors", label: "Errors" }] },
  { label: "Stats", pages: [{ id: "stats", label: "Stats" }] },
  { label: "Models", pages: [{ id: "models", label: "Models" }] },
  { label: "Copilot Auto", pages: [{ id: "auto", label: "Copilot Auto" }] },
  { label: "Smart Auto", pages: [{ id: "smart-auto", label: "Smart Auto" }] },
  { label: "Profiles", pages: [{ id: "profiles", label: "Profiles" }] },
  { label: "Settings", pages: SETTINGS_PAGES },
];

const PAGES: readonly Page[] = NAV_TABS.flatMap((navTab) => navTab.pages);
const DEFAULT_PAGE: Page = PAGES[0] ?? { id: "overview", label: "Overview" };
const ROUTE_ALIASES: Readonly<Record<string, string>> = { usage: "overview", settings: "keys" };

function overviewPeriod(requested: string | null): OverviewPeriod {
  return requested === "7d" ? "7d" : "1d";
}

interface ModelsResponse {
  readonly data: readonly HubModel[];
}

export function App() {
  const route = useRoute();
  const routeTab = route.path.split("/")[0] ?? "";
  const normalizedTab = ROUTE_ALIASES[routeTab] ?? routeTab;
  const page = PAGES.find((candidate) => candidate.id === normalizedTab) ?? DEFAULT_PAGE;
  const tab = page.id;
  const isSettings = SETTINGS_PAGES.some((candidate) => candidate.id === tab);
  const isInsights = INSIGHTS_PAGES.some((candidate) => candidate.id === tab);
  const groupPages = isSettings ? SETTINGS_PAGES : isInsights ? INSIGHTS_PAGES : null;
  const groupLabel = isSettings ? "Settings" : isInsights ? "Insights" : page.label;
  const [errorsToday, setErrorsToday] = useState<number | null>(null);
  const [meta, setMeta] = useState<HubMeta | null>(null);
  const [modelEntries, setModelEntries] = useState<readonly HubModel[]>([]);
  const models = modelEntries.map((model) => model.id);
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
      .then((body) => setModelEntries(body.data))
      .catch(handleError);
  }, [handleError]);

  useEffect(loadAll, [loadAll]);

  useEffect(() => {
    let cancelled = false;
    const loadErrorCount = async () => {
      try {
        const report = await api<ErrorsReport>("/api/errors?range=1d");
        if (!cancelled) {
          setErrorsToday(report.total_errors);
        }
      } catch (err) {
        if (cancelled) {
          return;
        }
        // Persistence off answers 400 here; the tab then shows no count.
        setErrorsToday(null);
        if (err instanceof UnauthorizedError) {
          setAskKey(true);
        }
      }
    };
    void loadErrorCount();
    return () => {
      cancelled = true;
    };
  }, [tab]);

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
        <header className="topbar">
          <div className="brand">
            <span className="brand-mark" aria-hidden="true" />
            llm-hub
            <span className="version">{meta ? `v${meta.version}` : ""}</span>
          </div>
          <nav className="topnav" aria-label="Main">
            {NAV_TABS.map((navTab) => {
              const target = navTab.pages[0] ?? DEFAULT_PAGE;
              const active = navTab.pages.some((candidate) => candidate.id === tab);
              return (
                <button
                  key={navTab.label}
                  type="button"
                  className={active ? "topnav-item active" : "topnav-item"}
                  aria-current={active ? "page" : undefined}
                  onClick={() => navigate(`/${target.id}`)}
                >
                  {navTab.label}
                  {target.id === "errors" && errorsToday !== null && errorsToday > 0 ? (
                    <Pill tone="danger" title="Errors today">
                      {formatNumber(errorsToday)}
                      <span className="sr-only"> today</span>
                    </Pill>
                  ) : null}
                </button>
              );
            })}
          </nav>
          <div className="topbar-actions">
            {meta ? (
              <>
                <Pill tone={meta.auth_enabled ? "ok" : "neutral"} dot>
                  {meta.auth_enabled ? "auth on" : "auth off"}
                </Pill>
                <Pill tone={meta.persistent ? "ok" : "warn"} dot>
                  {meta.persistent ? "persistent" : "in-memory"}
                </Pill>
              </>
            ) : null}
            <ThemeToggle />
            {meta ? (
              <button type="button" className="btn small danger" onClick={() => setConfirmRestart(true)}>
                <Icon name="restart" />
                Restart
              </button>
            ) : null}
          </div>
        </header>

        <div className="shell-body">
          <aside className="side" aria-label="Upstreams">
            <div className="side-title">Upstreams</div>
            <ul className="upstream-list">
              {(meta?.profiles ?? []).map((profile) => {
                const led = profile.healthy === true ? "up" : profile.healthy === false ? "down" : "off";
                const health = led === "up" ? "healthy" : led === "down" ? "unhealthy" : "unknown";
                return (
                  <li key={profile.name}>
                    <button
                      type="button"
                      className="upstream"
                      title={`${profile.label}: ${health}`}
                      onClick={() => navigate("/profiles")}
                    >
                      <span className={`led ${led}`} aria-hidden="true"></span>
                      <span className="upstream-name">{profile.label}</span>
                      <span className="sr-only">({health})</span>
                    </button>
                  </li>
                );
              })}
            </ul>
            {meta && meta.profiles.length === 0 ? <div className="upstream-empty">No profiles yet</div> : null}
          </aside>

          <main className="content">
            {tab === "overview" ? null : (
              <div className="page-head">
                <h1 className="page-title">{groupLabel}</h1>
              </div>
            )}
            {groupPages ? (
              <nav className="subtabs" aria-label={groupLabel}>
                {groupPages.map((candidate) => (
                  <button
                    key={candidate.id}
                    type="button"
                    className={candidate.id === tab ? "subtab active" : "subtab"}
                    aria-current={candidate.id === tab ? "page" : undefined}
                    onClick={() => navigate(`/${candidate.id}`)}
                  >
                    {candidate.label}
                  </button>
                ))}
              </nav>
            ) : null}
            {tab === "models" ? (
              <ModelsScreen
                models={modelEntries}
                profiles={meta?.profiles ?? []}
                meta={meta}
                onCopy={copy}
                onSaved={loadAll}
                onToast={toast}
              />
            ) : null}
            {tab === "stats" ? <StatsScreen onError={(message) => toast(message, true)} /> : null}
            {tab === "overview" ? <UsageScreen period={overviewPeriod(route.query.get("range"))} /> : null}
            {tab === "errors" ? <ErrorsScreen /> : null}
            {tab === "auto" ? <AutoScreen models={modelEntries} /> : null}
            {tab === "smart-auto" ? (
              <SmartAutoScreen profiles={meta?.profiles ?? []} prices={pricesById(modelEntries)} />
            ) : null}
            {tab === "insights" ? <InsightsScreen onCopy={copy} /> : null}
            {tab === "sessions" ? <SessionsScreen /> : null}
            {tab === "profiles" ? (
              <ProfilesScreen meta={meta} models={models} onChanged={loadAll} onToast={toast} />
            ) : null}
            {tab === "keys" ? <KeysScreen onToast={toast} authEnabled={meta?.auth_enabled} /> : null}
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

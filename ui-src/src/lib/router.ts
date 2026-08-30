import { useSyncExternalStore } from "react";

/** Fired after programmatic navigation so subscribers re-read the URL. */
const NAV_EVENT = "llm-hub:navigate";

export interface Route {
  /** Pathname without the leading slash, e.g. "profiles". */
  readonly path: string;
  readonly query: URLSearchParams;
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener("popstate", onChange);
  window.addEventListener(NAV_EVENT, onChange);
  return () => {
    window.removeEventListener("popstate", onChange);
    window.removeEventListener(NAV_EVENT, onChange);
  };
}

function snapshot(): string {
  return window.location.pathname + window.location.search;
}

/** Pushes a new history entry, e.g. navigate("/profiles"). */
export function navigate(path: string): void {
  window.history.pushState(null, "", path);
  window.dispatchEvent(new Event(NAV_EVENT));
}

/**
 * Sets or removes one query param on the current path without adding a
 * history entry — used by in-page filters so Back leaves the page instead
 * of stepping through every filter change.
 */
export function setQueryParam(name: string, value: string | null): void {
  const query = new URLSearchParams(window.location.search);
  if (value === null || value === "") {
    query.delete(name);
  } else {
    query.set(name, value);
  }
  const suffix = query.size > 0 ? `?${query.toString()}` : "";
  window.history.replaceState(null, "", `${window.location.pathname}${suffix}`);
  window.dispatchEvent(new Event(NAV_EVENT));
}

/** Current route, updated on Back/Forward and programmatic navigation. */
export function useRoute(): Route {
  const url = useSyncExternalStore(subscribe, snapshot);
  const [pathname = "", search = ""] = url.split("?");
  return {
    path: pathname.replace(/^\/+/, ""),
    query: new URLSearchParams(search),
  };
}

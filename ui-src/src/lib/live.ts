import { useEffect, useRef, useState, useSyncExternalStore } from "react";

export const LIVE_INTERVAL_MS = 15_000;
export const FLASH_MS = 2_400;
const PAUSE_KEY = "llm-hub-live-paused";

interface LiveState {
  readonly tick: number;
  readonly paused: boolean;
}

function readPaused(): boolean {
  try {
    return localStorage.getItem(PAUSE_KEY) === "1";
  } catch {
    return false;
  }
}

let state: LiveState = { tick: 0, paused: readPaused() };
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | null = null;

function publish(next: LiveState): void {
  state = next;
  listeners.forEach((listener) => listener());
}

function onTimer(): void {
  if (state.paused || document.hidden) {
    return;
  }
  publish({ ...state, tick: state.tick + 1 });
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  if (timer === null) {
    timer = setInterval(onTimer, LIVE_INTERVAL_MS);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}

function snapshot(): LiveState {
  return state;
}

export function toggleLivePaused(): void {
  const paused = !state.paused;
  try {
    localStorage.setItem(PAUSE_KEY, paused ? "1" : "0");
  } catch {
    // The pause still works for this tab when storage is blocked.
  }
  publish({ ...state, paused });
}

/** One shared timer for every screen. `tick` changes every 15 s while live and the tab is visible. */
export function useLive(): LiveState {
  return useSyncExternalStore(subscribe, snapshot);
}

/** Keys that appeared since the previous render of the list, for the 2.4 s highlight. */
export function useFreshKeys(keys: readonly string[]): ReadonlySet<string> {
  const known = useRef<ReadonlySet<string> | null>(null);
  const [fresh, setFresh] = useState<ReadonlySet<string>>(new Set());
  const signature = keys.join("|");

  useEffect(() => {
    const previous = known.current;
    known.current = new Set(keys);
    if (previous === null) {
      return;
    }
    const added = keys.filter((key) => !previous.has(key));
    if (added.length === 0) {
      return;
    }
    setFresh(new Set(added));
    const clear = setTimeout(() => setFresh(new Set()), FLASH_MS);
    return () => clearTimeout(clear);
    // `signature` stands for the whole key list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);

  return fresh;
}

/** True for 2.4 s after `value` changes. The first value never flashes. */
export function useChangeFlash(value: string): boolean {
  const previous = useRef<string | null>(null);
  const [flashing, setFlashing] = useState(false);

  useEffect(() => {
    const before = previous.current;
    previous.current = value;
    if (before === null || before === value) {
      return;
    }
    setFlashing(true);
    const clear = setTimeout(() => setFlashing(false), FLASH_MS);
    return () => clearTimeout(clear);
  }, [value]);

  return flashing;
}

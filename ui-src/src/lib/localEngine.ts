import type { LocalEngine } from "./types";

export const LOCAL_ENGINE_OPTIONS: readonly { readonly value: LocalEngine; readonly label: string }[] = [
  { value: "llama", label: "llama" },
  { value: "mistralrs", label: "mistral.rs" },
];

export function engineBadgeLabel(engine: LocalEngine): string {
  return engine === "llama" ? "llama" : "mistral.rs";
}

export function localEngineRoute(profile: string, model: string): string {
  return `/api/local/models/${encodeURIComponent(profile)}/${encodeURIComponent(model)}/engine`;
}

export function nextEngineSwitch(current: LocalEngine, next: string): LocalEngine | null {
  if (next === current) {
    return null;
  }
  if (next === "llama" || next === "mistralrs") {
    return next;
  }
  return null;
}

import { useEffect, useLayoutEffect, useState } from "react";

import { Segmented, type SegmentedOption } from "./Segmented";

type Theme = "light" | "dark";

const STORAGE_KEY = "llm-hub-theme";
const LIGHT_QUERY = "(prefers-color-scheme: light)";
const THEME_OPTIONS: readonly SegmentedOption<Theme>[] = [
  { value: "dark", label: "Dark" },
  { value: "light", label: "Light" },
];

function storedTheme(): Theme | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "light" || value === "dark" ? value : null;
  } catch {
    return null;
  }
}

function storeTheme(theme: Theme): void {
  try {
    localStorage.setItem(STORAGE_KEY, theme);
  } catch {
    // Storage can be blocked (private mode); the choice then lasts for this page only.
  }
}

function systemTheme(): Theme {
  return window.matchMedia(LIGHT_QUERY).matches ? "light" : "dark";
}

export function ThemeToggle() {
  const [chosen, setChosen] = useState<Theme | null>(storedTheme);
  const [system, setSystem] = useState<Theme>(systemTheme);
  const theme = chosen ?? system;

  useLayoutEffect(() => {
    if (chosen) {
      document.documentElement.dataset.theme = chosen;
      return;
    }
    delete document.documentElement.dataset.theme;
  }, [chosen]);

  useEffect(() => {
    const query = window.matchMedia(LIGHT_QUERY);
    const onChange = () => setSystem(query.matches ? "light" : "dark");
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);

  const choose = (value: Theme) => {
    storeTheme(value);
    setChosen(value);
  };

  return <Segmented label="Theme" options={THEME_OPTIONS} value={theme} onChange={choose} />;
}

import type { ReactNode } from "react";

export type PillTone = "neutral" | "ok" | "info" | "warn" | "danger";

interface PillProps {
  readonly tone?: PillTone;
  readonly dot?: boolean;
  readonly title?: string;
  readonly children: ReactNode;
}

export function Pill({ tone = "neutral", dot = false, title, children }: PillProps) {
  const className = tone === "neutral" ? "pill" : `pill pill-${tone}`;
  return (
    <span className={className} title={title}>
      {dot ? <span className="pill-dot" aria-hidden="true" /> : null}
      {children}
    </span>
  );
}

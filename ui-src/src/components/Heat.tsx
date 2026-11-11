import type { ReactNode } from "react";

import "./Heat.css";

const HEAT_WORDS: readonly string[] = ["good", "fine", "fair", "slow", "poor"];

interface HeatProps {
  /** 1 is the best value and 5 the worst. `null` renders the plain value. */
  readonly level: number | null;
  readonly children: ReactNode;
}

export function Heat({ level, children }: HeatProps) {
  if (level === null) {
    return <>{children}</>;
  }
  return (
    <span className={`heat heat-${level}`} title={HEAT_WORDS[level - 1]}>
      <span className="sr-only">{HEAT_WORDS[level - 1]}: </span>
      {children}
    </span>
  );
}

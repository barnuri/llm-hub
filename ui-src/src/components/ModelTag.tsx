import type { ReactNode } from "react";

import { nameColor } from "../lib/colors";
import "./ModelTag.css";

interface ModelTagProps {
  /** The text that picks the color, for example the model id without its profile prefix. */
  readonly colorKey: string;
  readonly children: ReactNode;
}

export function ModelTag({ colorKey, children }: ModelTagProps) {
  return (
    <span className="model-tag" style={{ ["--tag" as string]: nameColor(colorKey) }}>
      <span className="model-tag-dot" aria-hidden="true" />
      {children}
    </span>
  );
}

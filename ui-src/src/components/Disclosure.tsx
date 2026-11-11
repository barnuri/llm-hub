import { type ReactNode, useId, useState } from "react";

import "./Disclosure.css";

interface DisclosureProps {
  readonly title: string;
  readonly children: ReactNode;
  readonly className?: string;
}

export function Disclosure({ title, children, className = "" }: DisclosureProps) {
  const [open, setOpen] = useState(false);
  const regionId = useId();

  return (
    <section className={`panel disclosure ${className}`.trim()}>
      <button
        type="button"
        className="disclosure-trigger"
        aria-expanded={open}
        aria-controls={regionId}
        onClick={() => setOpen((current) => !current)}
      >
        <svg className="disclosure-chevron" viewBox="0 0 16 16" aria-hidden="true">
          <path d="m6 4 4 4-4 4" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
        {title}
      </button>
      {open ? (
        <div id={regionId} className="disclosure-body">
          {children}
        </div>
      ) : null}
    </section>
  );
}

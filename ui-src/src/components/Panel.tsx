import type { ReactNode } from "react";

interface PanelProps {
  readonly title?: string;
  readonly sub?: string;
  readonly actions?: ReactNode;
  readonly className?: string;
  readonly children: ReactNode;
}

export function Panel({ title, sub, actions, className, children }: PanelProps) {
  const hasHead = Boolean(title || sub || actions);
  return (
    <section className={className ? `panel ${className}` : "panel"}>
      {hasHead ? (
        <header className="panel-head">
          {title ? <h2 className="panel-title">{title}</h2> : null}
          {sub ? <span className="panel-sub">{sub}</span> : null}
          {actions ? <div className="panel-actions">{actions}</div> : null}
        </header>
      ) : null}
      {children}
    </section>
  );
}

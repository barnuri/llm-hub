import { type ReactNode, useEffect, useRef } from "react";

import "./InlineConfirm.css";

interface InlineConfirmProps {
  readonly title: ReactNode;
  readonly children: ReactNode;
  readonly confirmLabel: string;
  readonly onConfirm: () => void;
  readonly onCancel: () => void;
}

export function InlineConfirm({ title, children, confirmLabel, onConfirm, onCancel }: InlineConfirmProps) {
  const cancelButton = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    cancelButton.current?.focus();
  }, []);

  return (
    <section className="panel inline-confirm" role="alertdialog" aria-label={confirmLabel}>
      <p className="inline-confirm-text">
        <b>{title}</b> <span className="dim">{children}</span>
      </p>
      <div className="inline-confirm-actions">
        <button ref={cancelButton} type="button" className="btn small" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="btn small danger" onClick={onConfirm}>
          {confirmLabel}
        </button>
      </div>
    </section>
  );
}

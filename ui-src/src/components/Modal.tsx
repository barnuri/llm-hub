import type { ReactNode } from "react";

interface ModalProps {
  readonly title: string;
  readonly confirmLabel?: string;
  readonly danger?: boolean;
  readonly children: ReactNode;
  readonly onConfirm: () => void;
  readonly onCancel: () => void;
}

export function Modal({ title, confirmLabel = "Confirm", danger = false, children, onConfirm, onCancel }: ModalProps) {
  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-modal="true" aria-label={title}>
        <h3>{title}</h3>
        <div>{children}</div>
        <div className="modal-actions">
          <button type="button" className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button type="button" className={danger ? "btn danger" : "btn primary"} onClick={onConfirm}>
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}

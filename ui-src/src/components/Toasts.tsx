export interface ToastItem {
  readonly id: number;
  readonly message: string;
  readonly isError: boolean;
}

interface ToastsProps {
  readonly items: readonly ToastItem[];
}

export function Toasts({ items }: ToastsProps) {
  return (
    <div className="toasts" aria-live="polite">
      {items.map((toast) => (
        <div key={toast.id} className={toast.isError ? "toast error" : "toast"}>
          {toast.message}
        </div>
      ))}
    </div>
  );
}

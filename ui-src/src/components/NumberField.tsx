import "./NumberField.css";

interface NumberFieldProps {
  readonly value: string;
  readonly onChange: (value: string) => void;
  readonly ariaLabel: string;
  readonly min?: number;
  readonly step?: number;
  readonly inputMode?: "numeric" | "decimal";
  readonly invalid?: boolean;
  readonly disabled?: boolean;
  readonly className?: string;
}

const DEFAULT_STEP = 1;

export function NumberField({
  value,
  onChange,
  ariaLabel,
  min,
  step = DEFAULT_STEP,
  inputMode = "numeric",
  invalid = false,
  disabled = false,
  className = "",
}: NumberFieldProps) {
  const nudge = (direction: 1 | -1) => {
    const current = Number(value);
    const base = Number.isFinite(current) ? current : (min ?? 0);
    const next = Math.round((base + direction * step) * 1e6) / 1e6;
    onChange(String(min === undefined ? next : Math.max(min, next)));
  };

  return (
    <div className={`number-field ${className}`.trim()}>
      <input
        className="input number-field-input"
        type="number"
        min={min}
        step="any"
        inputMode={inputMode}
        aria-label={ariaLabel}
        aria-invalid={invalid}
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      />
      <div className="number-field-steps">
        <button type="button" tabIndex={-1} aria-label={`Increase ${ariaLabel}`} disabled={disabled} onClick={() => nudge(1)}>
          <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d="m4 9.5 4-4 4 4" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
        <button type="button" tabIndex={-1} aria-label={`Decrease ${ariaLabel}`} disabled={disabled} onClick={() => nudge(-1)}>
          <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d="m4 6.5 4 4 4-4" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>
      </div>
    </div>
  );
}

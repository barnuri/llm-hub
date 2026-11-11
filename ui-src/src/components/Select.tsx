import { type KeyboardEvent, useEffect, useId, useRef, useState } from "react";

import "./Select.css";

export interface SelectOption {
  readonly value: string;
  readonly label: string;
}

interface SelectProps {
  readonly value: string;
  readonly options: readonly SelectOption[];
  readonly onChange: (value: string) => void;
  readonly id?: string;
  readonly placeholder?: string;
  readonly ariaLabel?: string;
  readonly disabled?: boolean;
  readonly className?: string;
}

const LIST_MAX_HEIGHT = 280;

export function Select({
  value,
  options,
  onChange,
  id,
  placeholder = "",
  ariaLabel,
  disabled = false,
  className = "",
}: SelectProps) {
  const [open, setOpen] = useState(false);
  const [openUp, setOpenUp] = useState(false);
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const listId = useId();
  const selectedIndex = options.findIndex((option) => option.value === value);
  const label = options[selectedIndex]?.label ?? placeholder;

  useEffect(() => {
    if (!open) {
      return;
    }
    const closeOutside = (event: PointerEvent) => {
      if (event.target instanceof Node && !root.current?.contains(event.target)) {
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, [open]);

  useEffect(() => {
    if (open) {
      list.current?.querySelector(`[data-index="${active}"]`)?.scrollIntoView({ block: "nearest" });
    }
  }, [open, active]);

  const show = () => {
    const rect = root.current?.getBoundingClientRect();
    const below = rect ? window.innerHeight - rect.bottom : LIST_MAX_HEIGHT;
    setOpenUp(rect !== undefined && below < LIST_MAX_HEIGHT && rect.top > below);
    setActive(Math.max(selectedIndex, 0));
    setOpen(true);
  };

  const choose = (index: number) => {
    const option = options[index];
    if (option) {
      onChange(option.value);
    }
    setOpen(false);
  };

  const move = (delta: number) => {
    setActive((current) => Math.min(Math.max(current + delta, 0), options.length - 1));
  };

  const handleKey = (event: KeyboardEvent<HTMLButtonElement>) => {
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp":
        event.preventDefault();
        if (open) {
          move(event.key === "ArrowDown" ? 1 : -1);
        } else {
          show();
        }
        break;
      case "Home":
      case "End":
        if (open) {
          event.preventDefault();
          setActive(event.key === "Home" ? 0 : options.length - 1);
        }
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        if (open) {
          choose(active);
        } else {
          show();
        }
        break;
      case "Escape":
        if (open) {
          event.preventDefault();
          setOpen(false);
        }
        break;
      case "Tab":
        setOpen(false);
        break;
      default:
    }
  };

  return (
    <div ref={root} className="select">
      <button
        id={id}
        type="button"
        role="combobox"
        className={`input select-trigger ${className}`.trim()}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={listId}
        aria-activedescendant={open ? `${listId}-${active}` : undefined}
        aria-label={ariaLabel}
        disabled={disabled}
        onClick={() => (open ? setOpen(false) : show())}
        onKeyDown={handleKey}
      >
        <span className="select-label">{label}</span>
        <svg className="select-chevron" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M4 6.5 8 10.5 12 6.5" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      {open ? (
        <ul
          ref={list}
          id={listId}
          role="listbox"
          className={openUp ? "select-list up" : "select-list"}
          style={{ maxHeight: LIST_MAX_HEIGHT }}
        >
          {options.map((option, index) => (
            <li
              key={option.value}
              id={`${listId}-${index}`}
              role="option"
              data-index={index}
              aria-selected={index === selectedIndex}
              className={index === active ? "select-option active" : "select-option"}
              onPointerEnter={() => setActive(index)}
              onClick={() => choose(index)}
            >
              <span className="select-option-label">{option.label}</span>
              {index === selectedIndex ? (
                <svg className="select-check" viewBox="0 0 16 16" aria-hidden="true">
                  <path d="m3.5 8.5 3 3 6-7" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" />
                </svg>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

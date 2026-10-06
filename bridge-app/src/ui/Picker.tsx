// An expandable, searchable list for tokens and chains, which will both grow.

import { useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

export interface PickerOption<T extends string> {
  value: T;
  label: string;
  /// Extra text the search matches, such as a chain's full name or a token's address.
  keywords?: string;
  icon?: ReactNode;
  hint?: string;
}

export function Picker<T extends string>({
  value,
  options,
  onChange,
  placeholder,
  trigger,
  className = "",
}: {
  value: T;
  options: PickerOption<T>[];
  onChange: (v: T) => void;
  placeholder: string;
  trigger: ReactNode;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);

  useEffect(() => {
    if (!open) setQuery("");
    setCursor(0);
  }, [open, query]);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return options;
    return options.filter((o) => `${o.label} ${o.value} ${o.keywords ?? ""}`.toLowerCase().includes(q));
  }, [options, query]);

  const pick = (v: T) => {
    onChange(v);
    setOpen(false);
  };

  return (
    <div className={`picker-wrap ${className}`} ref={ref}>
      <button
        type="button"
        className={open ? "picker-trigger open" : "picker-trigger"}
        onClick={() => setOpen(!open)}
        aria-haspopup="listbox"
        aria-expanded={open}
      >
        {trigger}
        <svg className="caret" viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
          <path d="M6 9l6 6 6-6" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" />
        </svg>
      </button>
      {open && (
        <div className="picker">
          <input
            className="picker-search"
            autoFocus
            value={query}
            placeholder={placeholder}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setOpen(false);
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setCursor((c) => Math.min(c + 1, shown.length - 1));
              }
              if (e.key === "ArrowUp") {
                e.preventDefault();
                setCursor((c) => Math.max(c - 1, 0));
              }
              if (e.key === "Enter" && shown[cursor]) pick(shown[cursor].value);
            }}
          />
          <ul role="listbox">
            {shown.map((o, i) => (
              <li key={o.value}>
                <button
                  type="button"
                  role="option"
                  aria-selected={o.value === value}
                  className={[o.value === value ? "on" : "", i === cursor ? "cursor" : ""].join(" ")}
                  onMouseEnter={() => setCursor(i)}
                  onClick={() => pick(o.value)}
                >
                  {o.icon}
                  <span className="picker-label">{o.label}</span>
                  {o.hint && <span className="picker-hint">{o.hint}</span>}
                </button>
              </li>
            ))}
            {shown.length === 0 && <li className="picker-empty">No match</li>}
          </ul>
        </div>
      )}
    </div>
  );
}

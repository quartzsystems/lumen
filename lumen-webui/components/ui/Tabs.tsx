"use client";

import type React from "react";

export interface TabItem {
  value: string;
  label: string;
  /** Optional trailing count, rendered next to the label. */
  count?: number;
  /** A tab whose contents are not yet valid. Still reachable — see below. */
  invalid?: boolean;
}

/// Underline tab bar, ported from Quartz Command so both products' consoles
/// use the same control: a full-width bottom border with the active tab
/// underlined in the accent colour. `trailing` renders at the right edge
/// without breaking the underline.
///
/// Every tab is always reachable. A wizard that makes you walk forwards to
/// reach step five is a wizard you fight when you only wanted to change one
/// thing on step two; a tab that has something wrong on it says so with a mark
/// rather than by refusing to open.
export function Tabs({
  items,
  value,
  onChange,
  trailing,
  className = "",
}: {
  items: TabItem[];
  value: string;
  onChange: (v: string) => void;
  trailing?: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      role="tablist"
      className={`flex items-center gap-1 border-b border-[var(--qz-border)] overflow-x-auto ${className}`.trim()}
    >
      {items.map((it) => {
        const active = value === it.value;
        return (
          <button
            key={it.value}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(it.value)}
            className={[
              "px-3 py-2 text-[13px] font-medium border-b-2 -mb-px transition-colors cursor-pointer whitespace-nowrap bg-transparent",
              active
                ? "text-[var(--qz-accent)] border-[var(--qz-accent)]"
                : "text-[var(--qz-fg-3)] border-transparent hover:text-[var(--qz-fg-1)]",
            ].join(" ")}
          >
            {it.label}
            {it.count !== undefined && (
              <span className="ml-[6px] text-[12px] text-[var(--qz-fg-4)]">{it.count}</span>
            )}
            {it.invalid && (
              <span
                className="ml-[6px] text-[var(--qz-danger)]"
                aria-label="This tab has something that needs fixing"
                title="This tab has something that needs fixing"
              >
                ●
              </span>
            )}
          </button>
        );
      })}
      {trailing !== undefined && <div className="ml-auto flex items-center gap-2">{trailing}</div>}
    </div>
  );
}

"use client";

import type { ReactNode } from "react";
import { STATE_LABEL, STATE_TONE, type DomainState } from "@/lib/vmClient";

/// A machine's state, in the one place it is spelled.
export function StateBadge({ state }: { state: DomainState }) {
  return <span className={`badge badge-${STATE_TONE[state]}`}>{STATE_LABEL[state]}</span>;
}

/// A proportion an operator reads without stopping to divide. Turns amber past
/// three-quarters and red past nine-tenths, because that is when the number
/// stops being reassuring.
export function Meter({ percent, title }: { percent: number; title?: string }) {
  const value = Math.max(0, Math.min(100, Math.round(percent)));
  const tone = value >= 90 ? " qz-meter-crit" : value >= 75 ? " qz-meter-warn" : "";
  return (
    <div className={`qz-meter${tone}`} title={title ?? `${value}%`}>
      <div className="qz-meter-fill" style={{ width: `${value}%` }} />
    </div>
  );
}

/// The label/value grid every detail panel uses.
export function Facts({ children }: { children: ReactNode }) {
  return <dl className="qz-facts">{children}</dl>;
}

export function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

export function Mono({ children }: { children: ReactNode }) {
  return <span className="qz-mono">{children}</span>;
}

/// A panel with a heading — the section unit inside a detail page.
export function Panel({
  title,
  actions,
  children,
}: {
  title: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="surface p-5">
      <div className="flex items-center justify-between gap-3 mb-4">
        <h2 className="text-[14px] font-bold text-[var(--qz-fg-1)] m-0">{title}</h2>
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </div>
      {children}
    </section>
  );
}

export function Tags({ tags }: { tags: string[] }) {
  if (tags.length === 0) return <span className="qz-dim">—</span>;
  return (
    <span className="inline-flex flex-wrap gap-[6px]">
      {tags.map((tag) => (
        <span key={tag} className="badge badge-muted">
          {tag}
        </span>
      ))}
    </span>
  );
}

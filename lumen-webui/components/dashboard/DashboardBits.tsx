"use client";

import type { ReactNode } from "react";
import Link from "next/link";
import { ChevronRight } from "lucide-react";

/// The dashboard's own vocabulary: the tile across the top and the titled
/// panel below it. Both are here rather than in `components/vm/VmBits` because
/// neither is a detail-page part — a tile is a whole subsystem reduced to one
/// number, and a dashboard panel puts its table flush to its own edges, which
/// the detail-page `Panel` deliberately does not.

export type Tone = "ok" | "warn" | "crit" | "muted";

/// One subsystem, as a count and a proportion.
///
/// A tile is a link wherever there is a page that answers "which ones?" —
/// which is the question every number on this row provokes. The bar is always
/// rendered, empty where there is nothing to fill it, so a row of tiles keeps
/// one baseline whatever each of them knows.
export function StatTile({
  label,
  href,
  value,
  of,
  bar,
  note,
}: {
  label: string;
  /// Omitted where nothing owns this number yet — alarms are derived here and
  /// listed here, so there is nowhere to send anybody.
  href?: string;
  /// The number itself. A string rather than a number so a tile with nothing
  /// behind it can say so with the same weight.
  value: string;
  /// The denominator, where the number is a proportion of something.
  of?: string;
  bar?: ReactNode;
  /// One quiet line under the bar, for a tile that needs to explain itself.
  note?: string;
}) {
  const body = (
    <>
      <div className="flex items-center justify-between gap-2">
        <span className="kpi-label truncate">{label}</span>
        {href && (
          <ChevronRight
            size={14}
            className="flex-shrink-0 text-[var(--qz-fg-4)] group-hover:text-[var(--qz-fg-2)] transition-colors"
          />
        )}
      </div>
      <div
        className="mt-[10px] mb-[14px] text-[26px] font-semibold leading-none text-[var(--qz-fg-1)] truncate"
        style={{ fontFamily: "var(--qz-font-mono)", letterSpacing: "-0.02em" }}
      >
        {value}
        {of !== undefined && <span className="text-[var(--qz-fg-4)] font-normal"> / {of}</span>}
      </div>
      {/* Note above bar, and the pair pushed to the bottom together: tiles in
          a row stretch to the tallest, and a bar that sits at a different
          height on every tile is the first thing the eye catches. */}
      <div className="mt-auto">
        {note && (
          <div className="mb-[10px] text-[11px] text-[var(--qz-fg-4)] leading-snug">{note}</div>
        )}
        {bar ?? <Bar percent={0} />}
      </div>
    </>
  );

  const className = "kpi-card no-underline flex flex-col group";
  return href ? (
    <Link href={href} className={className}>
      {body}
    </Link>
  ) : (
    <div className={className}>{body}</div>
  );
}

/// A proportion, coloured by how much of it there is. Same thresholds as the
/// meter on a machine's memory, so the same fill means the same thing wherever
/// it appears.
export function Bar({ percent, tone }: { percent: number; tone?: Tone }) {
  const value = Math.max(0, Math.min(100, Math.round(percent)));
  const chosen: Tone = tone ?? (value >= 90 ? "crit" : value >= 75 ? "warn" : "ok");
  const modifier = chosen === "crit" ? " qz-meter-crit" : chosen === "warn" ? " qz-meter-warn" : "";
  return (
    <div className={`qz-meter${modifier}`}>
      <div className="qz-meter-fill" style={{ width: `${value}%` }} />
    </div>
  );
}

/// A titled box with its body flush to its edges, so a table inside it runs
/// the full width the way it does on a page of its own.
export function DashPanel({
  title,
  action,
  grow,
  children,
}: {
  title: string;
  /// Usually a link to the page that owns this data in full.
  action?: ReactNode;
  /// Take up whatever height is left in the column. Set on the panel that ends
  /// a column so two columns of unequal content still finish on the same line
  /// — a short column that stops early reads as a gap in the page rather than
  /// as the end of a column.
  grow?: boolean;
  children: ReactNode;
}) {
  return (
    <section className={`surface flex flex-col min-w-0${grow ? " flex-1" : ""}`}>
      <header className="flex items-center justify-between gap-3 px-5 py-[13px] border-b border-[var(--qz-border)]">
        <h2 className="text-[14px] font-bold text-[var(--qz-fg-1)] m-0 truncate">{title}</h2>
        {action}
      </header>
      <div className="min-w-0 overflow-x-auto">{children}</div>
    </section>
  );
}

/// The "and the rest of them are over here" link a panel ends with.
export function MoreLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <Link
      href={href}
      className="text-[12px] text-[var(--qz-accent)] no-underline inline-flex items-center gap-1 flex-shrink-0 hover:underline"
    >
      {children}
      <ChevronRight size={13} />
    </Link>
  );
}

/// A coloured dot with a word beside it — the shape every status cell in this
/// console uses.
export function Status({ tone, label }: { tone: Tone; label: string }) {
  return (
    <span className="inline-flex items-center gap-2 whitespace-nowrap">
      <span className={`state-dot state-dot-${tone}`} />
      <span className="text-[var(--qz-fg-2)]">{label}</span>
    </span>
  );
}

/// What a panel says when there is nothing in it. Kept to one sentence and the
/// same voice as `StubPage`, so an empty panel reads as empty rather than as
/// broken.
export function PanelEmpty({ children }: { children: ReactNode }) {
  return <div className="px-5 py-8 text-center text-[13px] text-[var(--qz-fg-4)]">{children}</div>;
}

/// A row that is not a link. The console's table styling assumes rows are
/// clickable, and most of the dashboard's are not.
export const staticRow = { cursor: "default" } as const;

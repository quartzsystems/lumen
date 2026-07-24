"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { ArrowRight, Search } from "lucide-react";
import { NAV, firstChildHref } from "@/lib/nav";

interface PaletteAction {
  id: string;
  section: string;
  label: string;
  href: string;
}

/// Every nav destination, flattened. Sections read "Networking › Interfaces"
/// so a child is findable by either half of its name.
const ACTIONS: PaletteAction[] = NAV.flatMap((item) => [
  { id: `nav-${item.id}`, section: "Go to", label: item.label, href: firstChildHref(item) },
  ...(item.children ?? []).map((child) => ({
    id: `nav-${item.id}-${child.id}`,
    section: "Go to",
    label: `${item.label} › ${child.label}`,
    href: child.href,
  })),
]);

/// Ctrl/Cmd-K jump-to-section palette.
export function CommandPalette({
  open,
  onClose,
  onNavigate,
}: {
  open: boolean;
  onClose: () => void;
  onNavigate: (href: string) => void;
}) {
  const [q, setQ] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) setTimeout(() => inputRef.current?.focus(), 50);
    else setQ("");
  }, [open]);

  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [onClose]);

  const grouped = useMemo(() => {
    const needle = q.toLowerCase();
    const filtered = ACTIONS.filter((a) => a.label.toLowerCase().includes(needle));
    return filtered.reduce<Record<string, PaletteAction[]>>((acc, a) => {
      (acc[a.section] = acc[a.section] || []).push(a);
      return acc;
    }, {});
  }, [q]);

  if (!open) return null;

  const empty = Object.keys(grouped).length === 0;

  return (
    <div className="palette-scrim" onClick={onClose}>
      <div className="palette" onClick={(e) => e.stopPropagation()}>
        <div
          className="flex items-center gap-[10px] p-[14px_18px]"
          style={{ borderBottom: "1px solid var(--qz-border)" }}
        >
          <Search size={16} className="text-[var(--qz-fg-3)]" />
          <input
            ref={inputRef}
            placeholder="Jump to a section…"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            className="flex-1 bg-transparent border-0 outline-none text-[var(--qz-fg-1)] text-[15px]"
            style={{ fontFamily: "var(--qz-font-sans)" }}
          />
          <span
            className="text-[10px] text-[var(--qz-fg-4)]"
            style={{ fontFamily: "var(--qz-font-mono)" }}
          >
            esc
          </span>
        </div>

        <div className="p-2 max-h-[50vh] overflow-auto">
          {Object.entries(grouped).map(([section, items]) => (
            <div key={section}>
              <div
                className="px-[10px] py-[6px] pb-[2px] text-[10px] tracking-[0.1em] text-[var(--qz-fg-4)] uppercase"
                style={{ fontFamily: "var(--qz-font-mono)" }}
              >
                {section}
              </div>
              {items.map((a) => (
                <div
                  key={a.id}
                  className="flex items-center gap-[10px] px-3 py-2 rounded-md text-[13.5px] text-[var(--qz-fg-2)] cursor-pointer hover:bg-[var(--qz-accent-soft)] hover:text-[var(--qz-fg-1)]"
                  onClick={() => {
                    onNavigate(a.href);
                    onClose();
                  }}
                >
                  <ArrowRight size={14} className="text-[var(--qz-fg-4)]" />
                  <span className="flex-1">{a.label}</span>
                </div>
              ))}
            </div>
          ))}
          {empty && <div className="p-5 text-[13px] text-[var(--qz-fg-3)]">No matches.</div>}
        </div>
      </div>
    </div>
  );
}

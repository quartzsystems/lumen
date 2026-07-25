"use client";

import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";

/// The console's second navigation column.
///
/// Deliberately a generic slot rather than anything about virtual machines:
/// Storage will want the same treatment, and the shell should only learn about
/// a third column once. A page puts something in it; `ConsoleShell` renders the
/// column only while something is there, so every page that ignores this keeps
/// the two-column layout it already had.
interface SecondaryNavState {
  content: ReactNode | null;
  setContent: (content: ReactNode | null) => void;
}

const SecondaryNavContext = createContext<SecondaryNavState | null>(null);

export function SecondaryNavProvider({ children }: { children: ReactNode }) {
  const [content, setContent] = useState<ReactNode | null>(null);
  return (
    <SecondaryNavContext.Provider value={{ content, setContent }}>
      {children}
    </SecondaryNavContext.Provider>
  );
}

/// Read by the shell. Pages use `useSecondaryNav` instead.
export function useSecondaryNavSlot() {
  const ctx = useContext(SecondaryNavContext);
  if (!ctx) throw new Error("useSecondaryNavSlot must be inside SecondaryNavProvider");
  return ctx;
}

/// Fill the column for as long as this component is mounted.
///
/// `render` is re-run only when `deps` change, and the column is emptied on
/// unmount — so navigating away from a page that had one leaves the next page
/// with the plain two-column shell rather than a stale sidebar.
export function useSecondaryNav(render: () => ReactNode, deps: React.DependencyList) {
  const ctx = useContext(SecondaryNavContext);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const node = useMemo(render, deps);
  const setContent = ctx?.setContent;

  useEffect(() => {
    if (!setContent) return;
    setContent(node);
    return () => setContent(null);
  }, [node, setContent]);
}

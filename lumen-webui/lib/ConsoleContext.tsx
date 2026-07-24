"use client";

import { createContext, useContext, useState } from "react";

interface ConsoleState {
  toast: string | null;
  setToast: (msg: string | null) => void;
}

const ConsoleContext = createContext<ConsoleState | null>(null);

/// Console-wide UI state shared by the shell and the pages inside it.
export function ConsoleProvider({ children }: { children: React.ReactNode }) {
  const [toast, setToast] = useState<string | null>(null);

  return (
    <ConsoleContext.Provider value={{ toast, setToast }}>{children}</ConsoleContext.Provider>
  );
}

export function useConsole() {
  const ctx = useContext(ConsoleContext);
  if (!ctx) throw new Error("useConsole must be inside ConsoleProvider");
  return ctx;
}

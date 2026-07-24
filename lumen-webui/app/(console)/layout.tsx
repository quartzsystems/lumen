"use client";

import { AuthGuard } from "@/components/AuthGuard";
import { ConsoleShell } from "@/components/console/ConsoleShell";

/// Shared chrome for every console page: auth gate + sidebar shell. The login
/// page lives outside this group and stays chrome-free.
export default function ConsoleLayout({ children }: { children: React.ReactNode }) {
  return (
    <AuthGuard>
      <ConsoleShell>{children}</ConsoleShell>
    </AuthGuard>
  );
}

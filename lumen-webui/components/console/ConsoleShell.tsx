"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Sidebar } from "@/components/console/Sidebar";
import { CommandPalette } from "@/components/console/CommandPalette";
import { Toast } from "@/components/console/Toast";
import { CheckpointBar } from "@/components/network/CheckpointBar";
import { ConsoleProvider, useConsole } from "@/lib/ConsoleContext";
import { NetworkCheckpointProvider } from "@/lib/NetworkCheckpointContext";

function Shell({ children }: { children: React.ReactNode }) {
  const router = useRouter();
  const { toast, setToast } = useConsole();
  const [paletteOpen, setPaletteOpen] = useState(false);

  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen(true);
      }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, []);

  return (
    <div
      className="h-screen overflow-hidden"
      style={{
        display: "grid",
        gridTemplateColumns: "240px 1fr",
        gridTemplateRows: "minmax(0, 1fr)",
      }}
    >
      <Sidebar onOpenPalette={() => setPaletteOpen(true)} />
      <main className="overflow-auto" style={{ background: "var(--qz-bg)" }}>
        {children}
      </main>

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        onNavigate={(href) => router.push(href)}
      />
      {toast && <Toast message={toast} onDismiss={() => setToast(null)} />}
      {/* Renders nothing unless a network change is waiting to be confirmed.
          It lives here rather than on the Interfaces page so its countdown
          survives navigating away from that page. */}
      <CheckpointBar />
    </div>
  );
}

/// Shared chrome for every console page: fixed-width sidebar plus a scrolling
/// content pane, with the command palette and toast host attached.
export function ConsoleShell({ children }: { children: React.ReactNode }) {
  return (
    <ConsoleProvider>
      <NetworkCheckpointProvider>
        <Shell>{children}</Shell>
      </NetworkCheckpointProvider>
    </ConsoleProvider>
  );
}

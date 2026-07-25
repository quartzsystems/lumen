"use client";

import { Suspense, useEffect, useState } from "react";
import { useSearchParams } from "next/navigation";
import { AuthGuard } from "@/components/AuthGuard";
import { ConsoleScreen } from "@/components/vm/VmConsole";
import { fetchVm, type VmView } from "@/lib/vmClient";

/// The console on its own, in a window of its own.
///
/// Deliberately outside the `(console)` group: this page is a screen and
/// nothing else. A sidebar and a page header in a window somebody opened to
/// watch a machine boot would be taking room from the only thing they opened
/// it for.
///
/// It reads the machine itself rather than sharing the console's list, because
/// a detached window is a separate document with no provider above it — one
/// request on open, and after that the viewer's own connection is the thing
/// that reports the machine's state.
export default function DetachedConsolePage() {
  return (
    <Suspense fallback={null}>
      <AuthGuard>
        <DetachedConsole />
      </AuthGuard>
    </Suspense>
  );
}

function DetachedConsole() {
  const params = useSearchParams();
  const vmParam = params.get("vm");
  const vmid = vmParam === null || vmParam === "" ? null : Number(vmParam);

  const [vm, setVm] = useState<VmView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (vmid === null || Number.isNaN(vmid)) {
      setError("This window was opened without a machine to show.");
      return;
    }
    let cancelled = false;
    fetchVm(vmid)
      .then((found) => !cancelled && setVm(found))
      .catch((err) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : "Could not read that machine.");
      });
    return () => {
      cancelled = true;
    };
  }, [vmid]);

  // The tab's own name, so an operator with three of these open can tell them
  // apart from the taskbar.
  useEffect(() => {
    if (vm) document.title = `${vm.name} — Lumen console`;
  }, [vm]);

  return (
    <main
      className="h-screen w-screen p-3 flex flex-col"
      style={{ background: "var(--qz-bg)" }}
    >
      {error ? (
        <div className="callout callout-crit m-auto">
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      ) : vm ? (
        <ConsoleScreen vmid={vm.vmid} name={vm.name} />
      ) : (
        <div className="m-auto text-[13px] text-[var(--qz-fg-4)]">Reading the machine…</div>
      )}
    </main>
  );
}

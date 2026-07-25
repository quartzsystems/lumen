"use client";

import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react";
import { ApiError } from "@/lib/authClient";
import { fetchVms, type NodeVms, type VmView } from "@/lib/vmClient";

/// The node's machines, read once for the whole console.
///
/// Machines are runtime data, not navigation: they change while the operator
/// is looking at them, and three separate consumers want them — the sidebar's
/// live list, the command palette, and the Virtual Machines page itself.
/// Polling once here and sharing the answer is what keeps that from being
/// three requests every five seconds.
interface VmState {
  nodes: NodeVms[];
  /// Every machine across every node, in identifier order.
  vms: VmView[];
  loading: boolean;
  /// Why the list is empty, when that is the reason. A node whose hypervisor
  /// is down still has a console, and it says so.
  error: string | null;
  refresh: () => Promise<void>;
  /// Stop polling while a dialog is open, so a refresh cannot yank a form out
  /// from under the operator mid-edit.
  setPaused: (paused: boolean) => void;
  /// Which machine the Virtual Machines page currently has open, so the
  /// sidebar can highlight it.
  ///
  /// Published from the page rather than read from the query string: the
  /// sidebar lives in the console layout, and `useSearchParams` there would
  /// force the whole layout behind a Suspense boundary at export time for the
  /// sake of one highlighted row.
  selected: number | null;
  setSelected: (vmid: number | null) => void;
}

const POLL_MS = 5000;

const VmContext = createContext<VmState | null>(null);

export function VmProvider({ children }: { children: React.ReactNode }) {
  const [nodes, setNodes] = useState<NodeVms[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const pausedRef = useRef(false);

  const refresh = useCallback(async () => {
    try {
      const response = await fetchVms();
      setNodes(response.nodes);
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login; anything else is worth saying
      // out loud rather than rendering as "no machines".
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the machines on this node.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const timer = setInterval(() => {
      if (!pausedRef.current) void refresh();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [refresh]);

  const vms = nodes.flatMap((node) => node.vms).sort((a, b) => a.vmid - b.vmid);

  return (
    <VmContext.Provider
      value={{
        nodes,
        vms,
        loading,
        error,
        refresh,
        setPaused: (paused) => {
          pausedRef.current = paused;
        },
        selected,
        setSelected,
      }}
    >
      {children}
    </VmContext.Provider>
  );
}

export function useVms() {
  const ctx = useContext(VmContext);
  if (!ctx) throw new Error("useVms must be inside VmProvider");
  return ctx;
}

/// The same list, for components that render outside the provider during a
/// prerender pass. Returns an empty list rather than throwing.
export function useVmsOptional(): VmState | null {
  return useContext(VmContext);
}

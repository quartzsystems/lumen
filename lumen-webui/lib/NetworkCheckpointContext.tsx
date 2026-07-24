"use client";

import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react";
import { ApiError } from "@/lib/authClient";
import {
  confirmApply,
  extendApply,
  fetchPending,
  rollbackApply,
  type CheckpointView,
} from "@/lib/networkClient";

/// Live state of an applied-but-unconfirmed network change.
///
/// This lives above the pages so the countdown survives navigating between
/// Overview and Interfaces: a change that reverts itself in sixty seconds must
/// not stop being visible because the operator clicked a link.
interface CheckpointState {
  checkpoint: CheckpointView | null;
  /// Seconds left, counted locally from the absolute deadline the server gave
  /// us. Never derived from a duration, so a reload or a slow request cannot
  /// drift it.
  secondsLeft: number;
  /// The node stopped answering. Expected — losing contact is the exact
  /// scenario the auto-revert exists for — so it is displayed, not treated as
  /// the end of the countdown.
  lostContact: boolean;
  busy: boolean;
  /// Adopt the checkpoint an apply just returned.
  begin: (checkpoint: CheckpointView) => void;
  /// Re-read the truth from the server. The server is the authority on
  /// whether a change survived; nothing here is ever persisted client-side.
  refresh: () => Promise<void>;
  confirm: () => Promise<void>;
  rollback: () => Promise<void>;
  extend: (seconds: number) => Promise<void>;
}

const NetworkCheckpointContext = createContext<CheckpointState | null>(null);

const secondsUntil = (deadline: number): number =>
  Math.max(0, Math.round(deadline - Date.now() / 1000));

export function NetworkCheckpointProvider({ children }: { children: React.ReactNode }) {
  const [checkpoint, setCheckpoint] = useState<CheckpointView | null>(null);
  const [secondsLeft, setSecondsLeft] = useState(0);
  const [lostContact, setLostContact] = useState(false);
  const [busy, setBusy] = useState(false);
  // Read inside the interval callbacks without making them re-subscribe.
  const outstanding = useRef(false);
  outstanding.current = checkpoint !== null;

  const adopt = useCallback((next: CheckpointView | null) => {
    setCheckpoint(next);
    setSecondsLeft(next ? secondsUntil(next.confirm_deadline) : 0);
  }, []);

  const refresh = useCallback(async () => {
    try {
      const pending = await fetchPending();
      setLostContact(false);
      adopt(pending.checkpoint);
    } catch (err) {
      // A network failure during the confirm window is not fatal and must not
      // clear the countdown: the node may be exactly as unreachable as the
      // revert is about to fix. A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 0 && outstanding.current) {
        setLostContact(true);
        return;
      }
      if (!(err instanceof ApiError) || err.status !== 401) {
        adopt(null);
      }
    }
  }, [adopt]);

  // Rehydrate on mount and on every reload — from the server, never from
  // localStorage. A checkpoint the browser thinks exists but the node does
  // not is worse than no countdown at all.
  useEffect(() => {
    void refresh();
  }, [refresh]);

  // The countdown itself: local, one second at a time, from the absolute
  // deadline. It keeps running while contact is lost — that is the number the
  // operator needs most right then.
  useEffect(() => {
    if (!checkpoint) return;
    const tick = setInterval(() => {
      setSecondsLeft(secondsUntil(checkpoint.confirm_deadline));
    }, 1000);
    return () => clearInterval(tick);
  }, [checkpoint]);

  // While a change is outstanding, keep asking the node how it is. This is
  // what resolves both endings: a confirm window that ran out (the server
  // reports no checkpoint) and a node that came back (contact restored, real
  // state re-read).
  useEffect(() => {
    if (!checkpoint) return;
    const poll = setInterval(() => {
      void refresh();
    }, 3000);
    return () => clearInterval(poll);
  }, [checkpoint, refresh]);

  const act = useCallback(
    async (action: () => Promise<unknown>) => {
      setBusy(true);
      try {
        await action();
        await refresh();
      } finally {
        setBusy(false);
      }
    },
    [refresh],
  );

  const value: CheckpointState = {
    checkpoint,
    secondsLeft,
    lostContact,
    busy,
    begin: adopt,
    refresh,
    confirm: () => act(confirmApply),
    rollback: () => act(rollbackApply),
    extend: (seconds: number) => act(() => extendApply(seconds)),
  };

  return (
    <NetworkCheckpointContext.Provider value={value}>{children}</NetworkCheckpointContext.Provider>
  );
}

export function useNetworkCheckpoint() {
  const ctx = useContext(NetworkCheckpointContext);
  if (!ctx) throw new Error("useNetworkCheckpoint must be inside NetworkCheckpointProvider");
  return ctx;
}

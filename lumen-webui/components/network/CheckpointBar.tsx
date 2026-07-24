"use client";

import { AlertTriangle, Check, Clock, RotateCcw, Timer } from "lucide-react";
import { useConsole } from "@/lib/ConsoleContext";
import { useNetworkCheckpoint } from "@/lib/NetworkCheckpointContext";

const mmss = (seconds: number): string => {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
};

/// The countdown for an applied-but-unconfirmed change.
///
/// Mounted in the console shell rather than on a page, so it stays put while
/// the operator navigates. Renders nothing when no change is outstanding.
export function CheckpointBar() {
  const { checkpoint, secondsLeft, lostContact, busy, confirm, rollback, extend } =
    useNetworkCheckpoint();
  const { setToast } = useConsole();

  if (!checkpoint) return null;

  const run = async (label: string, action: () => Promise<void>) => {
    try {
      await action();
      setToast(label);
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Something went wrong.");
    }
  };

  // Under a quarter of the window left is worth shouting about.
  const critical = secondsLeft <= Math.max(15, checkpoint.rollback_secs / 4);
  const progress = Math.max(0, Math.min(100, (secondsLeft / checkpoint.rollback_secs) * 100));

  return (
    <div className={`checkpoint-bar${critical || lostContact ? " checkpoint-bar-critical" : ""}`}>
      <div className="checkpoint-bar-fill" style={{ width: `${progress}%` }} />
      <div className="checkpoint-bar-body">
        {lostContact ? (
          <AlertTriangle size={16} className="flex-shrink-0 text-[var(--qz-danger)]" />
        ) : (
          <Clock size={16} className="flex-shrink-0 text-[var(--qz-accent)]" />
        )}
        <div className="min-w-0 flex-1">
          <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
            {lostContact
              ? `Lost contact with the node — it reverts automatically in ${mmss(secondsLeft)} unless you confirm.`
              : "Network changes applied. Confirm them to keep them."}
          </div>
          <div className="text-[12px] text-[var(--qz-fg-4)] mt-[2px]">
            {lostContact
              ? "Trying to reach the node again. If the change broke your path to it, doing nothing restores the previous configuration."
              : "If nobody confirms, the node restores the previous configuration by itself."}
          </div>
        </div>
        <span
          className={`checkpoint-countdown${critical ? " checkpoint-countdown-critical" : ""}`}
          title="Time until the node reverts on its own"
        >
          {mmss(secondsLeft)}
        </span>
        <button
          type="button"
          className="btn btn-ghost"
          disabled={busy || lostContact}
          onClick={() => run("Confirm window extended.", () => extend(120))}
        >
          <Timer size={14} />
          Extend
        </button>
        <button
          type="button"
          className="btn btn-danger"
          disabled={busy || lostContact}
          onClick={() => run("Changes rolled back.", rollback)}
        >
          <RotateCcw size={14} />
          Roll back
        </button>
        <button
          type="button"
          className="btn btn-primary"
          disabled={busy || lostContact}
          onClick={() => run("Changes confirmed.", confirm)}
        >
          <Check size={14} />
          Confirm
        </button>
      </div>
    </div>
  );
}

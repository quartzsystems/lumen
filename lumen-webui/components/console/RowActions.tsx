"use client";

import { useState } from "react";
import { Check, Pencil, Trash2, X } from "lucide-react";

/// Per-row edit/delete for the config tables, ported from QuartzFire. Delete
/// asks for inline confirmation before applying.
///
/// Either control can be switched off with a reason: Lumen's tables carry rows
/// it does not manage and rows something else depends on, and a control that
/// says why it is off is worth more than one that is silently grey.
export function RowActions({
  label,
  onEdit,
  onDelete,
  editDisabled = false,
  editTitle,
  deleteDisabled = false,
  deleteTitle,
}: {
  /** Accessible name of the row, e.g. `bridge vmbr0` or `rule 20`. */
  label: string;
  onEdit: () => void;
  onDelete: () => Promise<unknown>;
  editDisabled?: boolean;
  /** Replaces the default `Edit {label}` tooltip — say why it is off. */
  editTitle?: string;
  deleteDisabled?: boolean;
  /** Replaces the default `Delete {label}` tooltip — say why it is off. */
  deleteTitle?: string;
}) {
  const [confirming, setConfirming] = useState(false);
  const [working, setWorking] = useState(false);

  return (
    <div className="inline-flex items-center gap-1 justify-end">
      {confirming ? (
        <>
          <button
            type="button"
            title="Confirm delete"
            aria-label="Confirm delete"
            disabled={working}
            onClick={async () => {
              setWorking(true);
              try {
                await onDelete();
              } finally {
                setWorking(false);
                setConfirming(false);
              }
            }}
            className="grid place-items-center w-7 h-7 rounded-md border-0 cursor-pointer disabled:opacity-60"
            style={{ background: "var(--qz-danger)", color: "white" }}
          >
            <Check size={14} />
          </button>
          <button
            type="button"
            title="Cancel"
            aria-label="Cancel"
            onClick={() => setConfirming(false)}
            className="grid place-items-center w-7 h-7 rounded-md cursor-pointer text-[var(--qz-fg-3)] hover:text-[var(--qz-fg-1)]"
            style={{ background: "transparent", border: "1px solid var(--qz-border)" }}
          >
            <X size={14} />
          </button>
        </>
      ) : (
        <>
          <button
            type="button"
            title={editTitle ?? `Edit ${label}`}
            aria-label="Edit"
            disabled={editDisabled}
            onClick={onEdit}
            className="grid place-items-center w-7 h-7 rounded-md bg-transparent border-0 text-[var(--qz-fg-4)] hover:text-[var(--qz-accent)] hover:bg-[color-mix(in_oklab,white_5%,transparent)] transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-[var(--qz-fg-4)] disabled:hover:bg-transparent"
          >
            <Pencil size={14} />
          </button>
          <button
            type="button"
            title={deleteTitle ?? `Delete ${label}`}
            aria-label="Delete"
            disabled={deleteDisabled}
            onClick={() => setConfirming(true)}
            className="grid place-items-center w-7 h-7 rounded-md bg-transparent border-0 text-[var(--qz-fg-4)] hover:text-[var(--qz-danger)] hover:bg-[color-mix(in_oklab,white_5%,transparent)] transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-[var(--qz-fg-4)] disabled:hover:bg-transparent"
          >
            <Trash2 size={14} />
          </button>
        </>
      )}
    </div>
  );
}

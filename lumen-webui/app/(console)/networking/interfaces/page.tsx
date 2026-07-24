"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  Cable,
  ChevronDown,
  Network,
  Pencil,
  Plus,
  Share2,
  Tags,
  Trash2,
} from "lucide-react";
import { PageHeader } from "@/components/PageHeader";
import { LinkDialog, dialogKindFor, type DialogKind } from "@/components/network/LinkDialog";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { useNetworkCheckpoint } from "@/lib/NetworkCheckpointContext";
import {
  applyPending,
  convertManagementBridge,
  deleteLink,
  discardPending,
  fetchInterfaces,
  fetchPending,
  type InterfacesResponse,
  type LinkView,
  type PendingResponse,
} from "@/lib/networkClient";

const POLL_MS = 5000;

/// The node's network configuration, in the shape Proxmox's node network page
/// established: one table per node covering every link, with edits staged and
/// applied as a set rather than one at a time.
export default function InterfacesPage() {
  const { setToast } = useConsole();
  const { checkpoint, begin, refresh: refreshCheckpoint } = useNetworkCheckpoint();

  const [interfaces, setInterfaces] = useState<InterfacesResponse | null>(null);
  const [pending, setPending] = useState<PendingResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<{ kind: DialogKind; editing: LinkView | null } | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

  // Polling pauses while a dialog or the apply confirmation is open, so a
  // refresh cannot yank the form out from under the operator mid-edit.
  const paused = dialog !== null || confirming;
  const pausedRef = useRef(paused);
  pausedRef.current = paused;

  const load = useCallback(async () => {
    try {
      const [links, staged] = await Promise.all([fetchInterfaces(), fetchPending()]);
      setInterfaces(links);
      setPending(staged);
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login. A status 0 during a confirm
      // window is expected — the countdown in the shell is what matters then,
      // and it keeps running on its own.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the network configuration.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const timer = setInterval(() => {
      if (!pausedRef.current) void load();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  // A confirm, a rollback, or an expiry all change what the box looks like;
  // re-read as soon as the checkpoint goes away.
  useEffect(() => {
    if (!checkpoint) void load();
  }, [checkpoint, load]);

  const allLinks = interfaces?.nodes.flatMap((node) => node.interfaces) ?? [];
  const staged = pending?.changes ?? [];
  const management = allLinks.find((link) => link.management) ?? null;
  // Assume bridged until proven otherwise, so the banner never flashes up
  // during the first load.
  const managementIsBridged = management === null || management.kind === "bridge";
  const outstanding = checkpoint !== null;

  const run = async (action: () => Promise<unknown>, success: string) => {
    setBusy(true);
    try {
      await action();
      setToast(success);
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Something went wrong.");
    } finally {
      setBusy(false);
    }
  };

  const apply = async () => {
    setBusy(true);
    try {
      const response = await applyPending(pending?.requires_disconnect_ack ?? false);
      begin(response.checkpoint);
      setConfirming(false);
      setToast("Applied. Confirm before the window runs out.");
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not apply the changes.");
    } finally {
      setBusy(false);
    }
  };

  const convert = () =>
    run(async () => {
      const response = await convertManagementBridge();
      if (response.checkpoint) begin(response.checkpoint);
      await refreshCheckpoint();
    }, "Management bridge created. Confirm before the window runs out.");

  return (
    <div className="p-[28px_36px]">
      <PageHeader
        title="Interfaces"
        description="Physical adapters, bridges, bonds, and VLAN interfaces on this node."
        actions={
          <div className="relative">
            <button
              type="button"
              className="btn btn-primary"
              onClick={() => setMenuOpen((open) => !open)}
              disabled={outstanding}
              title={
                outstanding
                  ? "Confirm or roll back the outstanding change first"
                  : "Create an interface"
              }
            >
              <Plus size={14} />
              Create
              <ChevronDown size={14} />
            </button>
            {menuOpen && (
              <div className="menu">
                {(
                  [
                    { kind: "bridge", label: "Linux Bridge", Icon: Network },
                    { kind: "bond", label: "Linux Bond", Icon: Share2 },
                    { kind: "vlan", label: "Linux VLAN", Icon: Tags },
                  ] as const
                ).map(({ kind, label, Icon }) => (
                  <button
                    key={kind}
                    type="button"
                    className="menu-item"
                    onClick={() => {
                      setMenuOpen(false);
                      setDialog({ kind, editing: null });
                    }}
                  >
                    <Icon size={15} /> {label}
                  </button>
                ))}
              </div>
            )}
          </div>
        }
      />

      {error && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}

      {!managementIsBridged && management && (
        <div className="callout callout-warn mb-4">
          <Cable size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="flex-1 min-w-0">
            <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
              The management address is on a bare interface
            </div>
            <p className="text-[13px] text-[var(--qz-fg-3)] mt-1 mb-0">
              Virtual machines attach to a bridge, not to an adapter. Converting{" "}
              <span className="qz-mono">{management.name}</span> now moves the address onto{" "}
              <span className="qz-mono">br0</span> with the same addressing and the same hardware
              address, inside the usual confirm window — so a mistake reverts itself.
            </p>
          </div>
          <button
            type="button"
            className="btn btn-primary flex-shrink-0"
            onClick={convert}
            disabled={busy || outstanding || staged.length > 0}
            title={
              staged.length > 0
                ? "Apply or discard the staged changes first"
                : outstanding
                  ? "Confirm or roll back the outstanding change first"
                  : undefined
            }
          >
            Create management bridge
          </button>
        </div>
      )}

      {staged.length > 0 && (
        <div className="pending-bar mb-4">
          <span className="text-[13px] font-semibold text-[var(--qz-fg-1)] flex-1">
            {staged.length} staged {staged.length === 1 ? "change" : "changes"}, not yet applied
          </span>
          <button
            type="button"
            className="btn btn-ghost"
            disabled={busy || outstanding}
            onClick={() => run(discardPending, "Staged changes discarded.")}
          >
            Discard all
          </button>
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy || outstanding || (pending?.errors.length ?? 0) > 0}
            onClick={() => setConfirming(true)}
            title={
              (pending?.errors.length ?? 0) > 0
                ? "Fix the problems listed below first"
                : "Apply the staged changes"
            }
          >
            Apply configuration
          </button>
        </div>
      )}

      {(pending?.errors.length ?? 0) > 0 && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <ul className="m-0 pl-4 text-[13px] text-[var(--qz-fg-2)]">
            {pending?.errors.map((item) => (
              <li key={`${item.code}-${item.link ?? ""}`}>{item.message}</li>
            ))}
          </ul>
        </div>
      )}

      {interfaces === null && !error && (
        <div className="surface px-6 py-12 text-center text-[13px] text-[var(--qz-fg-4)]">
          Reading the network configuration…
        </div>
      )}

      {interfaces?.nodes.map((node) => (
        <section key={node.node} className="mb-6">
          <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] mb-2 qz-mono">
            {node.node}
          </h2>
          <InterfaceTable
            rows={node.interfaces}
            busy={busy || outstanding}
            onEdit={(link) => setDialog({ kind: dialogKindFor(link.kind), editing: link })}
            onDelete={(link) =>
              run(() => deleteLink(link.kind, link.name), `${link.name} staged for removal.`)
            }
          />
        </section>
      ))}

      {dialog && (
        <LinkDialog
          kind={dialog.kind}
          editing={dialog.editing}
          links={allLinks}
          managementLink={management?.name ?? null}
          onClose={() => setDialog(null)}
          onSaved={(next) => {
            setPending(next);
            setDialog(null);
            setToast("Change staged.");
            void load();
          }}
        />
      )}

      {confirming && (
        <ApplyDialog
          count={staged.length}
          seconds={pending?.checkpoint?.rollback_secs ?? null}
          requiresAck={pending?.requires_disconnect_ack ?? false}
          busy={busy}
          onCancel={() => setConfirming(false)}
          onApply={apply}
        />
      )}
    </div>
  );
}

const CHANGE_BADGE: Record<string, string> = {
  created: "badge badge-ok",
  modified: "badge badge-info",
  deleted: "badge badge-crit",
};

/// Rows arrive already ordered so a controller is immediately followed by its
/// ports (lumen-net orders them); the indent is what makes that visible.
function InterfaceTable({
  rows,
  busy,
  onEdit,
  onDelete,
}: {
  rows: LinkView[];
  busy: boolean;
  onEdit: (link: LinkView) => void;
  onDelete: (link: LinkView) => void;
}) {
  const dash = <span className="qz-dim">—</span>;
  return (
    <div className="qz-table-wrap">
      <table className="qz-table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Type</th>
            <th>State</th>
            <th>Ports</th>
            <th>Controller</th>
            <th>VLAN</th>
            <th>Bond mode</th>
            <th>MTU</th>
            <th>Address</th>
            <th>Gateway</th>
            <th>MAC</th>
            <th>Link</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {rows.map((link) => (
            <tr key={link.name}>
              <td className={link.controller ? "qz-indent" : undefined}>
                <span className="qz-mono text-[var(--qz-fg-1)] font-semibold">{link.name}</span>
                {link.management && <span className="badge badge-info ml-2">MGMT</span>}
                {link.change !== "unchanged" && (
                  <span className={`${CHANGE_BADGE[link.change]} ml-2`}>{link.change}</span>
                )}
              </td>
              <td className="qz-dim">{link.kind}</td>
              <td>
                <StateCell link={link} />
              </td>
              <td className="qz-mono">{link.ports.join(", ") || dash}</td>
              <td className="qz-mono">{link.controller ?? dash}</td>
              <td className="qz-mono">{link.vlan_id ?? dash}</td>
              <td className="qz-mono">{link.bond_mode ?? dash}</td>
              <td className="qz-mono">{link.mtu ?? dash}</td>
              <td className="qz-mono">{link.addresses.join(", ") || dash}</td>
              <td className="qz-mono">{link.gateway ?? dash}</td>
              <td
                className="qz-mono qz-dim"
                title={link.perm_mac ? `permanent ${link.perm_mac}` : undefined}
              >
                {link.mac ?? "—"}
              </td>
              <td className="qz-mono qz-dim">
                {link.speed_mbps
                  ? `${link.speed_mbps} Mb/s${link.duplex ? ` ${link.duplex}` : ""}`
                  : "—"}
              </td>
              <td>
                <div className="flex items-center gap-1 justify-end">
                  <button
                    type="button"
                    className="btn btn-sm btn-ghost"
                    disabled={busy || link.kind === "other"}
                    onClick={() => onEdit(link)}
                    title={link.kind === "other" ? "Not managed by Lumen" : `Edit ${link.name}`}
                  >
                    <Pencil size={13} />
                  </button>
                  <button
                    type="button"
                    className="btn btn-sm btn-ghost btn-danger"
                    disabled={busy || !link.deletable}
                    onClick={() => onDelete(link)}
                    // The control explains itself instead of being silently
                    // greyed out: the backend supplies the reason.
                    title={link.delete_blocked_reason ?? `Remove ${link.name}`}
                  >
                    <Trash2 size={13} />
                  </button>
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function StateCell({ link }: { link: LinkView }) {
  if (!link.present) return <span className="badge badge-muted">staged</span>;
  const up = link.oper_state === "activated";
  const coming = link.oper_state === "activating";
  return (
    <span className="inline-flex items-center gap-2">
      <span className={`badge ${up ? "badge-ok" : coming ? "badge-warn" : "badge-muted"}`}>
        {link.oper_state}
      </span>
      {link.kind === "ethernet" && (
        <span className={`badge ${link.carrier ? "badge-muted" : "badge-warn"}`}>
          {link.carrier ? "carrier" : "no carrier"}
        </span>
      )}
    </span>
  );
}

/// Says plainly what applying does — including that nobody confirming means it
/// all comes back.
function ApplyDialog({
  count,
  seconds,
  requiresAck,
  busy,
  onCancel,
  onApply,
}: {
  count: number;
  /// The window length as the API reports it — never a number hardcoded here.
  seconds: number | null;
  requiresAck: boolean;
  busy: boolean;
  onCancel: () => void;
  onApply: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const windowText = seconds !== null ? `${seconds} seconds` : "the confirm window";

  return (
    <div className="dialog-scrim" role="dialog" aria-modal="true">
      <div className="dialog">
        <h2 className="dialog-title">Apply network configuration</h2>
        <p className="dialog-subtitle">
          {count} {count === 1 ? "change" : "changes"} will be applied to this node.
        </p>
        <div className="callout callout-warn mt-5">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            The node applies the changes and then waits for you to confirm them. If nobody confirms
            within {windowText}, it restores the previous configuration by itself — so if a change
            cuts your connection, doing nothing brings the node back.
          </div>
        </div>
        {requiresAck && (
          <label className="checkbox-row mt-4">
            <input type="checkbox" checked={acked} onChange={(e) => setAcked(e.target.checked)} />
            This moves the management address and may disconnect me.
          </label>
        )}
        <div className="dialog-actions">
          <button type="button" className="btn" onClick={onCancel} disabled={busy}>
            Cancel
          </button>
          <button
            type="button"
            className="btn btn-primary"
            onClick={onApply}
            disabled={busy || (requiresAck && !acked)}
          >
            Apply
          </button>
        </div>
      </div>
    </div>
  );
}

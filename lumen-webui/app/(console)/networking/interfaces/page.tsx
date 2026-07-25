"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Cable,
  ChevronDown,
  Network,
  Plus,
  Share2,
  Tags,
} from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { RowActions } from "@/components/console/RowActions";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ModalFooter } from "@/components/ui/formkit";
import { LinkDialog, dialogKindFor, type DialogKind } from "@/components/network/LinkDialog";
import { ApiError } from "@/lib/authClient";
import { titleCase, titleCaseOptions } from "@/lib/labels";
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

  // The Create control sits in the table's own toolbar, next to Columns and
  // Refresh, rather than up in the page header: it acts on the table below it,
  // and every other control that does is already there.
  const createControl = (
    <div className="relative">
      <span
        title={outstanding ? "Confirm or roll back the outstanding change first" : undefined}
      >
        <Button
          kind="primary"
          size="sm"
          icon={Plus}
          iconRight={ChevronDown}
          onClick={() => setMenuOpen((open) => !open)}
          disabled={outstanding}
        >
          Create
        </Button>
      </span>
      {menuOpen && (
        <>
          {/* Click-away, so the menu closes the way every other one does. */}
          <div className="fixed inset-0 z-10" onClick={() => setMenuOpen(false)} />
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
        </>
      )}
    </div>
  );

  return (
    <Page>
      <PageHeader
        title="Interfaces"
        description="Physical adapters, bridges, bonds, and VLAN interfaces on this node."
      />

      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {!managementIsBridged && management && (
            <div className="callout callout-warn">
              <Cable size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="flex-1 min-w-0">
                <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
                  The management address is on a bare interface
                </div>
                <p className="text-[13px] text-[var(--qz-fg-3)] mt-1 mb-0">
                  Virtual machines attach to a bridge, not to an adapter. Converting{" "}
                  <span className="qz-mono">{management.name}</span> now moves the address onto{" "}
                  <span className="qz-mono">br0</span> with the same addressing and the same
                  hardware address, inside the usual confirm window — so a mistake reverts itself.
                </p>
              </div>
              <span
                className="flex-shrink-0"
                title={
                  staged.length > 0
                    ? "Apply or discard the staged changes first"
                    : outstanding
                      ? "Confirm or roll back the outstanding change first"
                      : undefined
                }
              >
                <Button
                  kind="primary"
                  onClick={convert}
                  disabled={busy || outstanding || staged.length > 0}
                >
                  Create management bridge
                </Button>
              </span>
            </div>
          )}

          {staged.length > 0 && (
            <div className="pending-bar">
              <span className="text-[13px] font-semibold text-[var(--qz-fg-1)] flex-1">
                {staged.length} staged {staged.length === 1 ? "change" : "changes"}, not yet applied
              </span>
              <Button
                kind="ghost"
                disabled={busy || outstanding}
                onClick={() => run(discardPending, "Staged changes discarded.")}
              >
                Discard all
              </Button>
              <span
                title={
                  (pending?.errors.length ?? 0) > 0
                    ? "Fix the problems listed below first"
                    : undefined
                }
              >
                <Button
                  kind="primary"
                  disabled={busy || outstanding || (pending?.errors.length ?? 0) > 0}
                  onClick={() => setConfirming(true)}
                >
                  Apply configuration
                </Button>
              </span>
            </div>
          )}

          {(pending?.errors.length ?? 0) > 0 && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <ul className="m-0 pl-4 text-[13px] text-[var(--qz-fg-2)]">
                {pending?.errors.map((item) => (
                  <li key={`${item.code}-${item.link ?? ""}`}>{item.message}</li>
                ))}
              </ul>
            </div>
          )}

          {interfaces === null && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">
              Reading the network configuration…
            </div>
          )}

          {interfaces?.nodes.map((node) => (
            <section key={node.node} className="flex flex-col gap-2">
              {/* One appliance is the usual case, and naming it then is noise.
                  A cluster is not, and then every table needs saying whose. */}
              {(interfaces?.nodes.length ?? 0) > 1 && (
                <h2
                  className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0"
                  style={{ fontFamily: "var(--qz-font-mono)" }}
                >
                  {node.node}
                </h2>
              )}
              <InterfaceTable
                rows={node.interfaces}
                busy={busy || outstanding}
                toolbar={createControl}
                onRefresh={load}
                onEdit={(link) => setDialog({ kind: dialogKindFor(link.kind), editing: link })}
                onDelete={(link) =>
                  run(() => deleteLink(link.kind, link.name), `${link.name} staged for removal.`)
                }
              />
            </section>
          ))}
        </div>
      </PageBody>

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
    </Page>
  );
}

const CHANGE_BADGE: Record<string, string> = {
  created: "badge badge-ok",
  modified: "badge badge-info",
  deleted: "badge badge-crit",
};

/// Whether the link is up on the box right now. A link that only exists in the
/// staged target has no answer yet, which is a third state and reads as one.
const activeText = (link: LinkView): string => {
  if (!link.present) return "staged";
  if (link.oper_state === "activating") return "coming up";
  return link.oper_state === "activated" ? "yes" : "no";
};

/// The addressing the configuration asks for. The box's own `addresses` are
/// what a running link reports; showing the desired value instead is what
/// makes a staged address visible before anyone applies it.
const addressText = (link: LinkView): string => {
  if (link.ip.mode === "dhcp") return "DHCP";
  if (link.ip.mode === "static") return link.ip.cidr;
  // Nothing configured, but the box may still hold a lease from elsewhere.
  return link.addresses.join(", ");
};

const gatewayText = (link: LinkView): string =>
  link.ip.mode === "static" ? link.ip.gateway : (link.gateway ?? "");

const columns: Column<LinkView>[] = [
  {
    key: "name",
    header: "Name",
    value: (link) => link.name,
    sortable: true,
    width: 170,
    render: (link) => (
      <span className="inline-flex items-center gap-2 min-w-0">
        <span
          className="text-[var(--qz-fg-1)] font-semibold truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {link.name}
        </span>
        {link.change !== "unchanged" && (
          <span className={CHANGE_BADGE[link.change]}>{link.change}</span>
        )}
      </span>
    ),
  },
  {
    key: "altname",
    header: "Alternative Name",
    value: (link) => link.altname ?? "",
    render: (link) => link.altname || <Dash />,
    mono: true,
    sortable: true,
    width: 150,
  },
  {
    key: "kind",
    header: "Type",
    value: (link) => link.kind,
    render: (link) => <span className="text-[var(--qz-fg-4)]">{titleCase(link.kind)}</span>,
    sortable: true,
    width: 100,
  },
  {
    key: "active",
    header: "Active",
    value: activeText,
    render: (link) => <ActiveCell link={link} />,
    sortable: true,
    width: 110,
  },
  {
    key: "vlan_aware",
    header: "VLAN Aware",
    value: (link) => (link.kind === "bridge" ? (link.vlan_aware ? "yes" : "no") : ""),
    render: (link) =>
      link.kind === "bridge" ? (
        <span className="text-[var(--qz-fg-4)]">{link.vlan_aware ? "Yes" : "No"}</span>
      ) : (
        <Dash />
      ),
    mono: true,
    width: 110,
  },
  {
    key: "ports",
    header: "Ports/Slaves",
    value: (link) => link.ports.join(", "),
    render: (link) =>
      link.ports.length > 0 ? (
        <span title={link.ports.join(", ")}>{link.ports.join(", ")}</span>
      ) : (
        <Dash />
      ),
    mono: true,
    width: 150,
  },
  {
    key: "bond_mode",
    header: "Bond Mode",
    value: (link) => link.bond_mode ?? "",
    render: (link) => link.bond_mode || <Dash />,
    mono: true,
    width: 130,
  },
  {
    key: "address",
    header: "Address",
    value: addressText,
    render: (link) => addressText(link) || <Dash />,
    mono: true,
    sortable: true,
    width: 170,
  },
  {
    key: "gateway",
    header: "Gateway",
    value: gatewayText,
    render: (link) => gatewayText(link) || <Dash />,
    mono: true,
    width: 150,
  },
  {
    key: "comment",
    header: "Description",
    value: (link) => link.comment ?? "",
    render: (link) =>
      link.comment ? (
        <span className="text-[var(--qz-fg-3)]" title={link.comment}>
          {link.comment}
        </span>
      ) : (
        <Dash />
      ),
    width: 220,
  },
];

/// One row per link on the node. Ports are listed against their controller
/// rather than nested under it: an operator scanning the Name column wants a
/// flat list of everything the box has.
function InterfaceTable({
  rows,
  busy,
  toolbar,
  onRefresh,
  onEdit,
  onDelete,
}: {
  rows: LinkView[];
  busy: boolean;
  toolbar?: React.ReactNode;
  onRefresh: () => Promise<void>;
  onEdit: (link: LinkView) => void;
  onDelete: (link: LinkView) => Promise<void>;
}) {
  // The drop-downs offer what is actually on this node, not every value the
  // API can produce — a filter for a type the box does not have is dead space.
  // The option value stays the wire one the predicate matches on; only the
  // label an operator reads is capitalised.
  const filters: FilterDef<LinkView>[] = useMemo(() => {
    const optionsOf = (of: (link: LinkView) => string) =>
      titleCaseOptions(Array.from(new Set(rows.map(of).filter(Boolean))).sort());
    return [
      {
        key: "kind",
        label: "Type",
        options: optionsOf((link) => link.kind),
        predicate: (link, value) => link.kind === value,
      },
      {
        key: "active",
        label: "Active",
        options: optionsOf(activeText),
        predicate: (link, value) => activeText(link) === value,
      },
    ];
  }, [rows]);

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      toolbar={toolbar}
      rowId={(link) => link.name}
      storageKey="networking-interfaces"
      searchPlaceholder="Search interfaces…"
      emptyMessage="No interfaces on this node."
      onRefresh={onRefresh}
      actions={(link) => (
        <RowActions
          label={link.name}
          onEdit={() => onEdit(link)}
          onDelete={() => onDelete(link)}
          editDisabled={busy || link.kind === "other"}
          editTitle={link.kind === "other" ? "Not managed by Lumen" : undefined}
          deleteDisabled={busy || !link.deletable}
          // The control explains itself instead of being silently greyed out:
          // the backend supplies the reason.
          deleteTitle={link.delete_blocked_reason ?? undefined}
        />
      )}
    />
  );
}

function ActiveCell({ link }: { link: LinkView }) {
  if (!link.present) return <span className="badge badge-muted">staged</span>;
  const up = link.oper_state === "activated";
  const coming = link.oper_state === "activating";
  return (
    <span
      className={`badge ${up ? "badge-ok" : coming ? "badge-warn" : "badge-muted"}`}
      // The full NetworkManager state is still one hover away.
      title={
        link.kind === "ethernet" && !link.carrier
          ? `${link.oper_state} · no carrier`
          : link.oper_state
      }
    >
      {titleCase(activeText(link))}
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
    <ModalShell onClose={onCancel}>
      <ModalHeader
        title="Apply network configuration"
        subtitle={`${count} ${count === 1 ? "change" : "changes"} will be applied to this node.`}
        onClose={onCancel}
      />

      <div className="flex flex-col gap-4">
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            The node applies the changes and then waits for you to confirm them. If nobody confirms
            within {windowText}, it restores the previous configuration by itself — so if a change
            cuts your connection, doing nothing brings the node back.
          </div>
        </div>

        {requiresAck && (
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <input
              type="checkbox"
              checked={acked}
              onChange={(e) => setAcked(e.target.checked)}
              style={{ accentColor: "var(--qz-accent)" }}
            />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              This moves the management address and may disconnect me.
            </span>
          </label>
        )}

        <ModalFooter
          onCancel={onCancel}
          saving={busy}
          disabled={requiresAck && !acked}
          savingLabel="Applying…"
          submitLabel="Apply"
          onSubmit={onApply}
        />
      </div>
    </ModalShell>
  );
}

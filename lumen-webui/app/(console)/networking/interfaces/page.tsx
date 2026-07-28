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
import { shortNodeName } from "@/lib/nodeNames";
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
import {
  fetchInventory,
  linksByMember,
  unreachable,
  type InventoryResponse,
  type OwnedLink,
} from "@/lib/inventoryClient";

const POLL_MS = 5000;

/// The node's network configuration, in the shape Proxmox's node network page
/// established: one table per node covering every link, with edits staged and
/// applied as a set rather than one at a time.
export default function InterfacesPage() {
  const { setToast } = useConsole();
  const { checkpoint, begin, refresh: refreshCheckpoint } = useNetworkCheckpoint();

  const [interfaces, setInterfaces] = useState<InterfacesResponse | null>(null);
  /// Every member's links. The node-local read above is still what the edit
  /// dialogs work against; this is what the table renders.
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
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
      const [links, staged, everyone] = await Promise.all([
        fetchInterfaces(),
        fetchPending(),
        // Settled rather than awaited with the rest: the environment read
        // reaches other nodes, and one member being away must not cost this
        // page the two reads that answered locally.
        fetchInventory().catch(() => null),
      ]);
      setInterfaces(links);
      setPending(staged);
      setInventory(everyone);
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

  // This node's own links: what the dialogs validate against (a bond may not
  // swallow a name that is taken *here*) and what the management banner reads.
  const allLinks = interfaces?.nodes.flatMap((node) => node.interfaces) ?? [];
  // Every member's, for the table. Falls back to this node alone when the
  // environment read failed, so the page still works standalone.
  const rows: OwnedLink[] =
    inventory !== null
      ? linksByMember(inventory)
      : allLinks.map((link) => ({ node: interfaces?.nodes[0]?.node ?? "", local: true, link }));
  const missing = unreachable(inventory);
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
        description="Physical adapters, bridges, bonds, and VLAN interfaces across the environment."
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

          {/* A member the environment knows about and could not be asked. Named
              rather than silently absent: a table quietly missing a node's rows
              reads as a node with no interfaces. */}
          {missing.length > 0 && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {missing.map((member) => shortNodeName(member.node)).join(", ")}{" "}
                {missing.length === 1 ? "could not be asked" : "could not be asked"} for its
                interfaces, so none are listed below.
                {missing[0]?.error && (
                  <span className="text-[var(--qz-fg-4)]"> {missing[0].error}</span>
                )}
              </div>
            </div>
          )}

          {interfaces !== null && (
            <InterfaceTable
              rows={rows}
              busy={busy || outstanding}
              toolbar={createControl}
              onRefresh={load}
              onEdit={(link) => setDialog({ kind: dialogKindFor(link.kind), editing: link })}
              onDelete={(link) =>
                run(() => deleteLink(link.kind, link.name), `${link.name} staged for removal.`)
              }
            />
          )}
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

const columns: Column<OwnedLink>[] = [
  {
    key: "node",
    header: "Node",
    value: (row) => row.node,
    sortable: true,
    width: 190,
    // The domain is the same on every row and the full name is one hover
    // away; see lib/nodeNames.ts.
    render: (row) => (
      <span className="qz-mono text-[12px] truncate" title={row.node}>
        {shortNodeName(row.node)}
      </span>
    ),
  },
  {
    key: "name",
    header: "Name",
    value: (row) => row.link.name,
    sortable: true,
    width: 170,
    render: (row) => (
      <span className="inline-flex items-center gap-2 min-w-0">
        <span
          className="text-[var(--qz-fg-1)] font-semibold truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {row.link.name}
        </span>
        {row.link.change !== "unchanged" && (
          <span className={CHANGE_BADGE[row.link.change]}>{row.link.change}</span>
        )}
      </span>
    ),
  },
  {
    key: "altname",
    header: "Alternative Name",
    value: (row) => row.link.altname ?? "",
    render: (row) => row.link.altname || <Dash />,
    mono: true,
    sortable: true,
    width: 150,
  },
  {
    key: "kind",
    header: "Type",
    value: (row) => row.link.kind,
    render: (row) => <span className="text-[var(--qz-fg-4)]">{titleCase(row.link.kind)}</span>,
    sortable: true,
    width: 100,
  },
  {
    key: "active",
    header: "Active",
    value: (row) => activeText(row.link),
    render: (row) => <ActiveCell link={row.link} />,
    sortable: true,
    width: 110,
  },
  {
    key: "vlan_aware",
    header: "VLAN Aware",
    value: (row) =>
      row.link.kind === "bridge" ? (row.link.vlan_aware ? "yes" : "no") : "",
    render: (row) =>
      row.link.kind === "bridge" ? (
        <span className="text-[var(--qz-fg-4)]">{row.link.vlan_aware ? "Yes" : "No"}</span>
      ) : (
        <Dash />
      ),
    mono: true,
    width: 110,
  },
  {
    key: "ports",
    header: "Ports/Slaves",
    value: (row) => row.link.ports.join(", "),
    render: (row) =>
      row.link.ports.length > 0 ? (
        <span title={row.link.ports.join(", ")}>{row.link.ports.join(", ")}</span>
      ) : (
        <Dash />
      ),
    mono: true,
    width: 150,
  },
  {
    key: "bond_mode",
    header: "Bond Mode",
    value: (row) => row.link.bond_mode ?? "",
    render: (row) => row.link.bond_mode || <Dash />,
    mono: true,
    width: 130,
  },
  {
    key: "address",
    header: "Address",
    value: (row) => addressText(row.link),
    render: (row) => addressText(row.link) || <Dash />,
    mono: true,
    sortable: true,
    width: 170,
  },
  {
    key: "gateway",
    header: "Gateway",
    value: (row) => gatewayText(row.link),
    render: (row) => gatewayText(row.link) || <Dash />,
    mono: true,
    width: 150,
  },
  {
    key: "comment",
    header: "Description",
    value: (row) => row.link.comment ?? "",
    render: (row) =>
      row.link.comment ? (
        <span className="text-[var(--qz-fg-3)]" title={row.link.comment}>
          {row.link.comment}
        </span>
      ) : (
        <Dash />
      ),
    width: 220,
  },
];

/// One row per link across every member. Ports are listed against their
/// controller rather than nested under it: an operator scanning the Name
/// column wants a flat list of everything the environment has.
///
/// One table rather than one per node, because the question an operator asks
/// here — "where does this address live?", "which node still has a free
/// port?" — is asked across the environment, and answering it by scrolling
/// between tables is the console making them do the join by hand.
///
/// Editing stays node-local. The links of another member are shown and not
/// touched: the writes behind that pencil land on whichever node serves the
/// request, so offering them on a remote row would quietly change the wrong
/// box.
function InterfaceTable({
  rows,
  busy,
  toolbar,
  onRefresh,
  onEdit,
  onDelete,
}: {
  rows: OwnedLink[];
  busy: boolean;
  toolbar?: React.ReactNode;
  onRefresh: () => Promise<void>;
  onEdit: (link: LinkView) => void;
  onDelete: (link: LinkView) => Promise<void>;
}) {
  // The drop-downs offer what is actually out there, not every value the API
  // can produce — a filter for a type nothing has is dead space. The option
  // value stays the wire one the predicate matches on; only the label an
  // operator reads is capitalised.
  const filters: FilterDef<OwnedLink>[] = useMemo(() => {
    const optionsOf = (of: (row: OwnedLink) => string) =>
      titleCaseOptions(Array.from(new Set(rows.map(of).filter(Boolean))).sort());
    return [
      {
        key: "node",
        label: "Node",
        // Node names are not titles and must not be capitalised — they are
        // what corosync matches on. The value stays the whole name for
        // exactly that reason; only the label is shortened.
        options: Array.from(new Set(rows.map((row) => row.node)))
          .sort()
          .map((node) => ({ value: node, label: shortNodeName(node) })),
        predicate: (row, value) => row.node === value,
      },
      {
        key: "kind",
        label: "Type",
        options: optionsOf((row) => row.link.kind),
        predicate: (row, value) => row.link.kind === value,
      },
      {
        key: "active",
        label: "Active",
        options: optionsOf((row) => activeText(row.link)),
        predicate: (row, value) => activeText(row.link) === value,
      },
    ];
  }, [rows]);

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      toolbar={toolbar}
      // Two members may each have a nic0; the node is what makes a row
      // identity unique across the environment.
      rowId={(row) => `${row.node}/${row.link.name}`}
      storageKey="networking-interfaces"
      searchPlaceholder="Search interfaces…"
      emptyMessage="No interfaces found."
      onRefresh={onRefresh}
      actions={(row) => (
        <RowActions
          label={row.link.name}
          onEdit={() => onEdit(row.link)}
          onDelete={() => onDelete(row.link)}
          editDisabled={busy || !row.local || row.link.kind === "other"}
          // Named by its owner rather than contrasted with wherever this
          // console happens to be pointed: which node is serving the page is
          // not something an operator should have to keep track of, and
          // "that node's console" only means anything if they do.
          editTitle={
            !row.local
              ? `Owned by ${shortNodeName(row.node)} — edit it on ${shortNodeName(row.node)}`
              : row.link.kind === "other"
                ? "Not managed by Lumen"
                : undefined
          }
          deleteDisabled={busy || !row.local || !row.link.deletable}
          // The control explains itself instead of being silently greyed out:
          // the backend supplies the reason for a local row.
          deleteTitle={
            !row.local
              ? `Owned by ${shortNodeName(row.node)} — delete it on ${shortNodeName(row.node)}`
              : (row.link.delete_blocked_reason ?? undefined)
          }
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

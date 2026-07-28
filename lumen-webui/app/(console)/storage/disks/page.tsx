"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Eraser } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { ModalFooter } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import {
  devicesByMember,
  fetchInventory,
  unreachable,
  wipeNodeDisk,
  type InventoryResponse,
  type OwnedDevice,
} from "@/lib/inventoryClient";
import { shortNodeName } from "@/lib/nodeNames";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 10000;

/// Every disk in the environment, and the one operation that makes a used one
/// usable again.
///
/// This page exists because the pool picker could only refuse. A disk carrying
/// an old partition table reads as spoken for — correctly — and until now the
/// only thing the console could do about it was grey the checkbox and print "2
/// partitions", which leaves an operator reusing hardware with nowhere to go
/// but a shell on the node.
///
/// Clearing is offered in exactly one state: something is on the disk, and
/// nothing live is using it. A disk holding a mount, swap, or an imported pool
/// is refused by the node that owns it — and that refusal is the node's, not
/// this page's, because the node is the only thing that can see what is
/// actually on it.
export default function DisksPage() {
  const { setToast } = useConsole();
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [wiping, setWiping] = useState<OwnedDevice | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setInventory(await fetchInventory());
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the disks.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // Polling pauses while the confirmation is open: a refresh that moved the
    // row out from under an operator about to erase a disk is the one place
    // that matters most.
    if (wiping) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, wiping]);

  const rows = useMemo(() => devicesByMember(inventory), [inventory]);
  const missing = unreachable(inventory);

  const wipe = async (target: OwnedDevice) => {
    setBusy(true);
    try {
      const cleared = await wipeNodeDisk(target.node, target.device.name);
      setWiping(null);
      setToast(
        cleared.in_use
          ? `${cleared.name} was cleared, and ${shortNodeName(target.node)} still reports it as in use — ${cleared.used_by ?? "something has it"}.`
          : `${cleared.name} is clear on ${shortNodeName(target.node)} and can hold a pool.`,
      );
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "The disk could not be cleared.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page>
      <PageHeader
        title="Disks"
        description="Every disk across the environment, what is on it, and whether a pool could be built on it."
      />

      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {inventory === null && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the disks…</div>
          )}

          {missing.length > 0 && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {missing.map((member) => shortNodeName(member.node)).join(", ")} could not be asked,
                so {missing.length === 1 ? "its disks are" : "their disks are"} not listed below.
                {missing[0]?.error && (
                  <span className="text-[var(--qz-fg-4)]"> {missing[0].error}</span>
                )}
              </div>
            </div>
          )}

          {inventory !== null && (
            <DiskTable rows={rows} busy={busy} onRefresh={load} onWipe={setWiping} />
          )}
        </div>
      </PageBody>

      {wiping && (
        <WipeDiskDialog
          target={wiping}
          busy={busy}
          onClose={() => setWiping(null)}
          onConfirm={() => void wipe(wiping)}
        />
      )}
    </Page>
  );
}

/// What is on the disk, and whether that is something an operator can undo.
///
/// Three states worth telling apart, not two. "Free" is a disk a pool can be
/// built on now; "reclaimable" is one carrying a partition table nobody is
/// using, which is a decision away from free; "in use" is a disk something has
/// open, which is not this page's to take.
const diskState = (row: OwnedDevice): { tone: string; label: string } => {
  if (!row.device.in_use) return { tone: "ok", label: "Free" };
  if (row.device.wipeable) return { tone: "warn", label: "Reclaimable" };
  return { tone: "muted", label: "In use" };
};

const columns: Column<OwnedDevice>[] = [
  {
    key: "state",
    header: "Status",
    value: (row) => diskState(row).label,
    sortable: true,
    width: 140,
    render: (row) => {
      const state = diskState(row);
      return <span className={`badge badge-${state.tone}`}>{state.label}</span>;
    },
  },
  {
    key: "node",
    header: "Node",
    value: (row) => row.node,
    sortable: true,
    width: 150,
    render: (row) => (
      <span className="qz-mono text-[12px] truncate" title={row.node}>
        {shortNodeName(row.node)}
      </span>
    ),
  },
  {
    key: "name",
    header: "Name",
    value: (row) => row.device.name,
    sortable: true,
    width: 130,
    render: (row) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {row.device.name}
      </span>
    ),
  },
  {
    key: "size",
    header: "Size",
    value: (row) => row.device.size,
    sortable: true,
    width: 110,
    render: (row) => <span className="qz-mono">{formatBytes(row.device.size)}</span>,
  },
  {
    key: "model",
    header: "Model",
    value: (row) => row.device.model ?? "",
    render: (row) => row.device.model || <Dash />,
    sortable: true,
    width: 220,
  },
  {
    key: "kind",
    header: "Kind",
    value: (row) => (row.device.rotational ? "spinning" : "solid state"),
    sortable: true,
    width: 130,
    render: (row) => (
      <span className="text-[var(--qz-fg-4)]">
        {row.device.rotational ? "Spinning" : "Solid state"}
      </span>
    ),
  },
  {
    key: "used_by",
    header: "Contents",
    value: (row) => row.device.used_by ?? "",
    // The node's own words. "in pool tank" and "2 partitions" are the same
    // cell and completely different decisions, and paraphrasing either would
    // lose the difference.
    render: (row) =>
      row.device.used_by ? (
        <span className="truncate" title={row.device.used_by}>
          {row.device.used_by}
        </span>
      ) : (
        <span className="qz-dim">empty</span>
      ),
    sortable: true,
    width: 220,
  },
  {
    key: "path",
    header: "Path",
    value: (row) => row.device.path,
    // The stable path, because that is what a pool is actually built on —
    // `/dev/sdb` is whatever the kernel enumerated second this boot.
    render: (row) => (
      <span className="truncate qz-dim" title={`${row.device.path}\n${row.device.kernel_path}`}>
        {row.device.path}
      </span>
    ),
    mono: true,
    width: 300,
  },
];

function DiskTable({
  rows,
  busy,
  onRefresh,
  onWipe,
}: {
  rows: OwnedDevice[];
  busy: boolean;
  onRefresh: () => Promise<void>;
  onWipe: (row: OwnedDevice) => void;
}) {
  const filters: FilterDef<OwnedDevice>[] = useMemo(
    () => [
      {
        key: "node",
        label: "Node",
        options: Array.from(new Set(rows.map((row) => row.node)))
          .sort()
          .map((node) => ({ value: node, label: shortNodeName(node) })),
        predicate: (row, value) => row.node === value,
      },
      {
        key: "state",
        label: "Status",
        options: Array.from(new Set(rows.map((row) => diskState(row).label)))
          .sort()
          .map((label) => ({ value: label, label })),
        predicate: (row, value) => diskState(row).label === value,
      },
    ],
    [rows],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      // Two members may each have an nvme0n1; the node is what makes a row
      // identity unique across the environment.
      rowId={(row) => `${row.node}/${row.device.name}`}
      storageKey="storage-disks"
      searchPlaceholder="Search disks…"
      emptyMessage="No disks found."
      onRefresh={onRefresh}
      actionsWidth={90}
      actions={(row) => (
        // The control says why it is unavailable rather than being silently
        // grey — and the two reasons are opposite, so they get different
        // sentences: a disk with nothing on it needs no clearing, and one
        // something is using must not be cleared.
        <span
          title={
            row.device.wipeable
              ? "Clear this disk's partition table and signatures"
              : row.device.in_use
                ? `In use — ${row.device.used_by ?? "something has it open"}. Free it first.`
                : "Already empty"
          }
        >
          <Button
            kind="ghost"
            size="sm"
            icon={Eraser}
            disabled={busy || !row.device.wipeable}
            onClick={() => onWipe(row)}
          />
        </span>
      )}
    />
  );
}

/// Says plainly what clearing does, and does not overstate it.
///
/// The distinction the last paragraph draws is the honest one: this removes
/// the labels that named the data, not the data. The disk reads as empty and
/// can hold a pool; the blocks are still there until something writes over
/// them. A dialog that promised erasure would be lying to an operator about to
/// hand a disk to somebody else.
function WipeDiskDialog({
  target,
  busy,
  onClose,
  onConfirm,
}: {
  target: OwnedDevice;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const { device, node } = target;

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title={`Clear ${device.name}`}
        subtitle={`${formatBytes(device.size)} on ${shortNodeName(node)}${device.model ? ` — ${device.model}` : ""}`}
        onClose={busy ? () => {} : onClose}
      />

      <div className="flex flex-col gap-4">
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            <span className="qz-mono">{device.name}</span> carries{" "}
            <span className="qz-mono">{device.used_by ?? "a partition table"}</span>. Clearing it
            removes the partition table and every filesystem and pool signature on the disk.
            Whatever those partitions held becomes unreachable, and there is no undo.
          </div>
        </div>

        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          This clears the labels that named the data, not the data itself — the blocks stay until
          something writes over them. What it changes is that{" "}
          <span className="qz-mono">{shortNodeName(node)}</span> will report the disk as empty, so
          a pool can be built on it.
        </p>

        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <input
            type="checkbox"
            checked={acked}
            onChange={(e) => setAcked(e.target.checked)}
            style={{ accentColor: "var(--qz-accent)" }}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand this may lose data on <span className="qz-mono">{device.name}</span>.
          </span>
        </label>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!acked}
          submitLabel="Clear disk"
          savingLabel="Clearing…"
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
  );
}

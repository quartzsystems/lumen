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

/// A clearing job: the disks, and how to drop the ticks that chose them.
///
/// The selection is the table's, so releasing it is the table's too — the
/// callback rides along with the disks rather than being reached for later,
/// because by the time the work finishes the bar it came from may be gone.
interface WipeJob {
  targets: OwnedDevice[];
  clear?: () => void;
}

/// Every disk in the environment, and the one operation that makes a used one
/// usable again.
///
/// This page exists because the pool picker could only refuse. A disk carrying
/// an old partition table reads as spoken for — correctly — and until now the
/// only thing the console could do about it was grey the checkbox and print "2
/// partitions", which leaves an operator reusing hardware with nowhere to go
/// but a shell on the node.
///
/// Clearing is offered on any disk nothing live is using, the ones that
/// already read as empty included: `/sys` counts partitions and cannot see a
/// signature, so a disk whose label was damaged rather than removed looks
/// blank here and is still refused by `zpool create`. A disk holding a mount,
/// swap, or an imported pool is refused by the node that owns it — and that
/// refusal is the node's, not this page's, because the node is the only thing
/// that can see what is actually on it.
///
/// Several at once, because that is how disks arrive. A tray of drives out of
/// an old array is one decision an operator makes and twelve confirmations
/// they should not have to sit through — so the selection the table already
/// had gets an action, and the disks are cleared one at a time behind it, each
/// reported on its own.
export default function DisksPage() {
  const { setToast } = useConsole();
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [wiping, setWiping] = useState<WipeJob | null>(null);
  const [done, setDone] = useState(0);
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

  /// One disk at a time, and one refusal does not stop the rest.
  ///
  /// Serial rather than all at once: each of these is a node writing to a
  /// block device, and a dozen of them in flight is a dozen ways for a partly
  /// finished job to be hard to read afterwards. A disk the node refuses is
  /// recorded and the next one is still attempted — the operator picked
  /// twelve disks, and stopping at the third because something has it open
  /// would leave nine untouched for no reason.
  const wipe = async (job: WipeJob) => {
    setBusy(true);
    setDone(0);
    const cleared: string[] = [];
    const refused: string[] = [];

    for (const target of job.targets) {
      const where = `${target.device.name} on ${shortNodeName(target.node)}`;
      try {
        const disk = await wipeNodeDisk(target.node, target.device.name);
        // Cleared, and the node still says something has it: worth saying
        // rather than counting as a success, because the disk will refuse a
        // pool for the same reason it did before.
        if (disk.in_use) refused.push(`${where} — ${disk.used_by ?? "something still has it"}`);
        else cleared.push(where);
      } catch (err) {
        refused.push(`${where} — ${err instanceof Error ? err.message : "it could not be cleared"}`);
      }
      setDone((n) => n + 1);
    }

    setWiping(null);
    setBusy(false);
    setToast(summarise(cleared, refused));
    // Only once the work is over: a tick that vanishes while the disk it
    // chose is still being cleared reads as the job having been abandoned.
    job.clear?.();
    await load();
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
        <WipeDisksDialog
          targets={wiping.targets}
          busy={busy}
          done={done}
          onClose={() => setWiping(null)}
          onConfirm={() => void wipe(wiping)}
        />
      )}
    </Page>
  );
}

/// What happened, in one sentence, however many disks it took.
///
/// A disk that was refused is named with the node's reason attached, because
/// "3 of 4 cleared" without the fourth's name is a sentence that sends an
/// operator back to the table to work out which one — and the reason is the
/// node's own words, which is the only account of the disk worth having.
const summarise = (cleared: string[], refused: string[]): string => {
  if (refused.length === 0) {
    return cleared.length === 1
      ? `${cleared[0]} is clear and can hold a pool.`
      : `${cleared.length} disks are clear and can hold a pool.`;
  }
  if (cleared.length === 0) {
    return refused.length === 1
      ? `${refused[0]}.`
      : `No disk was cleared. ${refused.join(". ")}.`;
  }
  return `${cleared.length} of ${cleared.length + refused.length} disks cleared. ${refused.join(". ")}.`;
};

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

/// Why this disk cannot be cleared, or nothing when it can.
///
/// One sentence, used by both the row's control and the selection bar, so the
/// two never disagree about a disk.
const refusalFor = (row: OwnedDevice): string | null =>
  row.device.wipeable
    ? null
    : `In use — ${row.device.used_by ?? "something has it open"}. Free it first.`;

function DiskTable({
  rows,
  busy,
  onRefresh,
  onWipe,
}: {
  rows: OwnedDevice[];
  busy: boolean;
  onRefresh: () => Promise<void>;
  onWipe: (job: WipeJob) => void;
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
      // The whole selection, cleared in one confirmation. Disks the node
      // would refuse are dropped here rather than being sent and failing
      // twelve times — the count on the button is what will actually be
      // attempted, and the dialog says what was left out.
      bulkActions={(picked, clear) => {
        const clearable = picked.filter((row) => row.device.wipeable);
        return (
          <span
            title={
              clearable.length === 0
                ? "None of the selected disks can be cleared — each is in use."
                : `Clear ${clearable.length} of the ${picked.length} selected`
            }
          >
            <Button
              kind="secondary"
              size="sm"
              icon={Eraser}
              disabled={busy || clearable.length === 0}
              onClick={() => onWipe({ targets: clearable, clear })}
            >
              Clear {clearable.length === 1 ? "disk" : `${clearable.length} disks`}
            </Button>
          </span>
        );
      }}
      actions={(row) => (
        // The control says why it is unavailable rather than being silently
        // grey. Offered on a disk that reads as empty too: the node cannot
        // see a signature, only a partition count, and a disk carrying a
        // damaged label looks exactly like a blank one until a pool build
        // refuses it.
        <span
          title={
            refusalFor(row) ??
            (row.device.in_use
              ? "Clear this disk's partition table and signatures"
              : "Clear any signature left on this disk — one that reads as empty can still carry an old label")
          }
        >
          <Button
            kind="ghost"
            size="sm"
            icon={Eraser}
            disabled={busy || !row.device.wipeable}
            onClick={() => onWipe({ targets: [row] })}
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
///
/// One disk or twelve is the same dialog. A batch is the case where an
/// operator is most likely to be wrong about what they picked, so every disk
/// is listed with the node it is on and what is on it — the confirmation has
/// to be readable, not just counted.
function WipeDisksDialog({
  targets,
  busy,
  done,
  onClose,
  onConfirm,
}: {
  targets: OwnedDevice[];
  busy: boolean;
  /// How many of them the node has been through, so a batch that takes a
  /// while says so rather than sitting on "Clearing…".
  done: number;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const one = targets.length === 1 ? targets[0] : null;
  const total = targets.reduce((sum, row) => sum + row.device.size, 0);
  const nodes = Array.from(new Set(targets.map((row) => row.node)));

  return (
    <ModalShell onClose={busy ? () => {} : onClose} maxWidth={one ? 520 : 620}>
      <ModalHeader
        title={one ? `Clear ${one.device.name}` : `Clear ${targets.length} disks`}
        subtitle={
          one
            ? `${formatBytes(one.device.size)} on ${shortNodeName(one.node)}${one.device.model ? ` — ${one.device.model}` : ""}`
            : `${formatBytes(total)} across ${nodes.length === 1 ? shortNodeName(nodes[0]) : `${nodes.length} nodes`}`
        }
        onClose={busy ? () => {} : onClose}
      />

      <div className="flex flex-col gap-4">
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {one ? (
              <>
                <span className="qz-mono">{one.device.name}</span> carries{" "}
                <span className="qz-mono">{one.device.used_by ?? "no partition table the node can see"}</span>.
                Clearing it removes the partition table and every filesystem and pool signature on
                the disk. Whatever it held becomes unreachable, and there is no undo.
              </>
            ) : (
              <>
                Every disk below has its partition table and every filesystem and pool signature
                removed. Whatever they held becomes unreachable, and there is no undo.
              </>
            )}
          </div>
        </div>

        {!one && (
          // Named, not counted. The number on the button is what an operator
          // checks against the table; this is what they check against the
          // rack.
          <div
            className="surface flex flex-col overflow-y-auto"
            style={{ maxHeight: 220 }}
          >
            {targets.map((row) => (
              <div
                key={`${row.node}/${row.device.name}`}
                className="flex items-center gap-3 px-3 py-2"
                style={{ borderTop: "1px solid var(--qz-border)" }}
              >
                <span className="qz-mono text-[12px] text-[var(--qz-fg-4)] w-[110px] truncate" title={row.node}>
                  {shortNodeName(row.node)}
                </span>
                <span className="qz-mono text-[12px] text-[var(--qz-fg-1)] font-semibold w-[90px]">
                  {row.device.name}
                </span>
                <span className="text-[12px] text-[var(--qz-fg-4)] w-[80px]">
                  {formatBytes(row.device.size)}
                </span>
                <span className="text-[12px] text-[var(--qz-fg-3)] truncate" title={row.device.used_by ?? undefined}>
                  {row.device.used_by ?? "nothing the node can see"}
                </span>
              </div>
            ))}
          </div>
        )}

        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          This clears the labels that named the data, not the data itself — the blocks stay until
          something writes over them. What it changes is that{" "}
          {one ? <span className="qz-mono">{shortNodeName(one.node)}</span> : "each node"} will
          report {one ? "the disk" : "its disks"} as empty, so a pool can be built on{" "}
          {one ? "it" : "them"}. A disk that already reads as empty is worth clearing for the same
          reason: a damaged label is invisible here and is exactly what a pool build refuses.
        </p>

        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <input
            type="checkbox"
            checked={acked}
            onChange={(e) => setAcked(e.target.checked)}
            style={{ accentColor: "var(--qz-accent)" }}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand this may lose data on{" "}
            {one ? (
              <span className="qz-mono">{one.device.name}</span>
            ) : (
              `all ${targets.length} disks`
            )}
            .
          </span>
        </label>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!acked}
          submitLabel={one ? "Clear disk" : `Clear ${targets.length} disks`}
          savingLabel={one ? "Clearing…" : `Clearing ${Math.min(done + 1, targets.length)} of ${targets.length}…`}
          onSubmit={onConfirm}
        />
      </div>
    </ModalShell>
  );
}

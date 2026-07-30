"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Camera, Trash2 } from "lucide-react";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { CheckRow } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import {
  deletePoolSnapshot,
  fetchPooledStorage,
  POOL_HEALTH_LABEL,
  POOL_HEALTH_TONE,
  replicationLabel,
  rollbackPoolDisk,
  takePoolSnapshot,
  type PooledStorageView,
  type PoolMember,
  type PoolVdisk,
} from "@/lib/poolClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 5000;

/// The LumenFS pool: pooled, deduplicated storage owned by the cluster,
/// serving each machine disk as the same `/dev/ublkb<id>` on every member.
///
/// The section owns its own data and renders nothing at all on a node with
/// no pool — absent rather than empty, the same shape the replicated-volume
/// section takes. The one exception is a pool configuration that exists but
/// could not be assembled: that is a broken deployment, and it renders as
/// its own sentence, because "nothing to show" and "your deployment is
/// broken" must never look alike.
export function PooledStorageSection() {
  const { setToast } = useConsole();
  const [pool, setPool] = useState<PooledStorageView | null>(null);
  const [broken, setBroken] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [snapshotting, setSnapshotting] = useState<PoolVdisk | null>(null);

  const load = useCallback(async () => {
    try {
      const response = await fetchPooledStorage();
      setPool(response.pool);
      setBroken(response.error);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the pool.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // Polling pauses while the dialog is open, so a refresh cannot move
    // the snapshot list out from under the operator mid-choice.
    if (snapshotting) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, snapshotting]);

  // Keep the dialog's row current across reloads: it shows the snapshot
  // list, which its own actions change.
  useEffect(() => {
    if (!snapshotting || !pool) return;
    const fresh = pool.vdisks.find((v) => v.vdisk === snapshotting.vdisk);
    if (fresh && fresh !== snapshotting) setSnapshotting(fresh);
  }, [pool, snapshotting]);

  if (!pool && !broken && !error) return null;

  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-center gap-3">
        <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
          Pooled Storage{pool ? ` — ${pool.name}` : ""}
        </h2>
        {pool && (
          <span className={`badge badge-${POOL_HEALTH_TONE[pool.health]}`}>
            {POOL_HEALTH_LABEL[pool.health]}
          </span>
        )}
      </div>

      {(error ?? broken) && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error ?? broken}</div>
        </div>
      )}

      {pool && (
        <>
          <DataTable
            rows={pool.members}
            columns={memberColumns}
            rowId={(member) => member.name}
            emptyMessage="No members answered."
          />
          <DataTable
            rows={pool.vdisks}
            columns={vdiskColumns(pool)}
            rowId={(vdisk) => String(vdisk.vdisk)}
            emptyMessage="The pool holds no disks yet — they are created with a machine."
            actions={(vdisk) => (
              <span title="Snapshots: instant, crash-consistent, on both members at once">
                <Button
                  kind="ghost"
                  size="sm"
                  icon={Camera}
                  onClick={() => setSnapshotting(vdisk)}
                />
              </span>
            )}
          />
        </>
      )}

      {snapshotting && (
        <PoolSnapshotsDialog
          vdisk={snapshotting}
          onClose={() => setSnapshotting(null)}
          onChanged={load}
          onRolledBack={() => {
            setToast(`${diskLabel(snapshotting)} rolled back on every member.`);
            setSnapshotting(null);
            void load();
          }}
        />
      )}
    </section>
  );
}

/// The compute domain's name for a disk, or the bare id for one that is not
/// a machine's — shown rather than hidden, because the pool is carrying it.
const diskLabel = (vdisk: PoolVdisk): string =>
  vdisk.disk ? `vm-${vdisk.disk.vmid}-disk-${vdisk.disk.index}` : `vdisk ${vdisk.vdisk}`;

const memberColumns: Column<PoolMember>[] = [
  {
    key: "name",
    header: "Member",
    value: (member) => member.name,
    sortable: true,
    width: 160,
    render: (member) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {member.name}
      </span>
    ),
  },
  {
    key: "state",
    header: "Replication",
    value: (member) => ("Answered" in member.view ? 0 : 1),
    width: 200,
    render: (member) => {
      if ("Silent" in member.view) {
        // A member that does not answer is presented, not dropped — with
        // its reason, because "unreachable: connection refused" is
        // actionable and "unknown" is not.
        return (
          <span className="badge badge-crit" title={member.view.Silent}>
            Unreachable
          </span>
        );
      }
      const { label, tone } = replicationLabel(member.view.Answered.replication);
      return <span className={`badge badge-${tone}`}>{label}</span>;
    },
  },
  {
    key: "era",
    header: "Era",
    value: (member) => ("Answered" in member.view ? member.view.Answered.era : -1),
    width: 70,
    render: (member) =>
      "Answered" in member.view ? (
        <span className="qz-mono">{member.view.Answered.era}</span>
      ) : (
        <Dash />
      ),
  },
  {
    key: "free",
    header: "Brick free",
    value: (member) =>
      "Answered" in member.view
        ? member.view.Answered.segments_free / Math.max(member.view.Answered.segments_total, 1)
        : -1,
    width: 130,
    render: (member) => {
      if (!("Answered" in member.view)) return <Dash />;
      const status = member.view.Answered;
      if (status.segments_total === 0) return <Dash />;
      const percent = Math.round((status.segments_free * 100) / status.segments_total);
      return (
        <span className={`qz-mono ${percent < 15 ? "text-[var(--qz-danger)]" : ""}`}>
          {percent}% of {status.segments_total} segments
        </span>
      );
    },
  },
  {
    key: "writes",
    header: "Writes",
    value: (member) =>
      "Answered" in member.view ? (member.view.Answered.accepts_writes ? 0 : 1) : 2,
    width: 90,
    render: (member) =>
      "Answered" in member.view ? (
        member.view.Answered.accepts_writes ? (
          <span className="text-[var(--qz-fg-3)]">open</span>
        ) : (
          <span className="text-[var(--qz-danger)]">held</span>
        )
      ) : (
        <Dash />
      ),
  },
];

const vdiskColumns = (pool: PooledStorageView): Column<PoolVdisk>[] => [
  {
    key: "name",
    header: "Disk",
    value: (vdisk) => diskLabel(vdisk),
    sortable: true,
    width: 170,
    render: (vdisk) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {diskLabel(vdisk)}
      </span>
    ),
  },
  {
    key: "device",
    header: "Device",
    value: (vdisk) => vdisk.device,
    width: 140,
    render: (vdisk) => <span className="qz-mono">{vdisk.device}</span>,
  },
  {
    key: "size",
    header: "Size",
    value: (vdisk) => vdisk.size_bytes,
    sortable: true,
    width: 100,
    render: (vdisk) => <span className="qz-mono">{formatBytes(vdisk.size_bytes)}</span>,
  },
  {
    key: "holder",
    header: "Writer",
    value: (vdisk) => vdisk.holder ?? -1,
    width: 150,
    render: (vdisk) => {
      if (vdisk.holder === null) return <Dash />;
      // A lease names an engine node id; the member's name is what an
      // operator recognises.
      const holder = memberNamed(pool, vdisk.holder);
      if (vdisk.handing_to !== null) {
        return (
          <span className="badge badge-warn">
            {holder} → {memberNamed(pool, vdisk.handing_to)}
          </span>
        );
      }
      return <span className="qz-mono">{holder}</span>;
    },
  },
  {
    key: "served",
    header: "Served on",
    value: (vdisk) => vdisk.exported_on.length,
    width: 150,
    render: (vdisk) =>
      vdisk.exported_on.length === 0 ? (
        <Dash />
      ) : (
        <span className="qz-mono">{vdisk.exported_on.join(", ")}</span>
      ),
  },
  {
    key: "snapshots",
    header: "Snapshots",
    value: (vdisk) => vdisk.snapshots.length,
    width: 90,
    render: (vdisk) =>
      vdisk.snapshots.length === 0 ? (
        <Dash />
      ) : (
        <span className="qz-mono">{vdisk.snapshots.length}</span>
      ),
  },
];

const memberNamed = (pool: PooledStorageView, node: number): string =>
  pool.members.find(
    (member) => "Answered" in member.view && member.view.Answered.node === node,
  )?.name ?? `node ${node}`;

const formatSnapshotMoment = (snapshot: number): string => {
  // The console mints ids as Unix seconds so they sort and read as the
  // moments they are; an id somebody minted by hand still shows as itself.
  const plausible = snapshot > 1_000_000_000 && snapshot < 100_000_000_000;
  return plausible ? new Date(snapshot * 1000).toLocaleString() : `#${snapshot}`;
};

function PoolSnapshotsDialog({
  vdisk,
  onClose,
  onChanged,
  onRolledBack,
}: {
  vdisk: PoolVdisk;
  onClose: () => void;
  onChanged: () => Promise<void>;
  onRolledBack: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The rollback confirmation, opened per-snapshot.
  const [rollingBack, setRollingBack] = useState<number | null>(null);
  const [acked, setAcked] = useState(false);

  const run = async (work: () => Promise<void>, fallback: string) => {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : fallback);
    } finally {
      setBusy(false);
    }
  };

  const take = () =>
    run(async () => {
      // The id is the moment: Unix seconds, so the list reads as history.
      await takePoolSnapshot(diskLabel(vdisk), Math.floor(Date.now() / 1000));
      await onChanged();
    }, "The snapshot could not be taken.");

  const remove = (snapshot: number) =>
    run(async () => {
      await deletePoolSnapshot(diskLabel(vdisk), snapshot);
      if (rollingBack === snapshot) setRollingBack(null);
      await onChanged();
    }, "The snapshot could not be deleted.");

  const rollback = () =>
    run(async () => {
      await rollbackPoolDisk(diskLabel(vdisk), rollingBack ?? 0);
      onRolledBack();
    }, "The rollback did not complete.");

  return (
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title={`Snapshots of ${diskLabel(vdisk)}`}
        subtitle="A retained point in the disk's history — instant, crash-consistent, and on every member at once because the pool replicates it like any other write."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        <div>
          <button
            type="button"
            className="btn btn-primary btn-sm"
            disabled={busy}
            onClick={() => void take()}
          >
            {busy ? "Working…" : "Take snapshot now"}
          </button>
        </div>

        {vdisk.snapshots.length === 0 && (
          <div className="text-[13px] text-[var(--qz-fg-4)]">
            No snapshots yet. Each is named for the moment it was taken.
          </div>
        )}

        {vdisk.snapshots.length > 0 && (
          <ul className="m-0 p-0 flex flex-col gap-2" style={{ listStyle: "none" }}>
            {[...vdisk.snapshots].reverse().map((snapshot) => (
              <li key={snapshot.snapshot} className="surface p-3 flex flex-col gap-3">
                <div className="flex items-center gap-3">
                  <span className="qz-mono text-[13px] text-[var(--qz-fg-1)] flex-1 truncate">
                    {formatSnapshotMoment(snapshot.snapshot)}
                  </span>
                  <span className="text-[12px] text-[var(--qz-fg-4)]">
                    {formatBytes(snapshot.size_bytes)}
                  </span>
                  <button
                    type="button"
                    className="btn btn-ghost btn-sm"
                    disabled={busy}
                    onClick={() =>
                      setRollingBack(rollingBack === snapshot.snapshot ? null : snapshot.snapshot)
                    }
                  >
                    Roll back…
                  </button>
                  <span title="Delete this snapshot on every member">
                    <Button
                      kind="ghost"
                      size="sm"
                      icon={Trash2}
                      onClick={() => void remove(snapshot.snapshot)}
                    />
                  </span>
                </div>

                {rollingBack === snapshot.snapshot && (
                  <div className="flex flex-col gap-3 border-t border-[var(--qz-border)] pt-3">
                    <p className="text-[12px] text-[var(--qz-fg-3)] m-0">
                      The disk rewinds to this moment and everything written since is discarded —
                      on every member, because the rollback replicates like the writes it
                      discards. The machine using this disk must be powered off; the pool
                      refuses while any member is serving the device.
                    </p>
                    <CheckRow checked={acked} onChange={() => setAcked(!acked)}>
                      I understand everything written since this snapshot is lost.
                    </CheckRow>
                    <div className="flex justify-end gap-2">
                      <button
                        type="button"
                        className="btn btn-ghost btn-sm"
                        onClick={() => setRollingBack(null)}
                      >
                        Cancel
                      </button>
                      <button
                        type="button"
                        className="btn btn-danger btn-sm"
                        disabled={busy || !acked}
                        onClick={() => void rollback()}
                      >
                        {busy ? "Rolling back…" : "Roll back"}
                      </button>
                    </div>
                  </div>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </ModalShell>
  );
}

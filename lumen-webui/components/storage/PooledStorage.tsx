"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Camera, Plus, Trash2 } from "lucide-react";
import { ProgressRow } from "@/components/cluster/CreateClusterDialog";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { CreateLumenPoolDialog } from "@/components/storage/CreateLumenPoolDialog";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { CheckRow } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { fetchEnvironment } from "@/lib/clusterClient";
import { fetchReplicatedVolumes } from "@/lib/drbdClient";
import {
  deletePoolSnapshot,
  destroyLumenPool,
  fetchPooledStorage,
  fetchPoolPending,
  POOL_HEALTH_LABEL,
  POOL_HEALTH_TONE,
  replicationLabel,
  rollbackPoolDisk,
  takePoolSnapshot,
  type PooledStorageView,
  type PoolMember,
  type PoolProgress,
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
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState(false);
  /// Whether the empty-state create card may render: this node is in a
  /// cluster and the cluster runs no DRBD volumes — the engine
  /// exclusivity presented, not just enforced server-side.
  const [eligible, setEligible] = useState(false);

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

  // Read once: whether a create is even offerable here. Both facts change
  // only through workflows that reload this page anyway.
  useEffect(() => {
    void (async () => {
      try {
        const [environment, replicated] = await Promise.all([
          fetchEnvironment(),
          fetchReplicatedVolumes(),
        ]);
        const clustered = environment.clusters.some((cluster) =>
          cluster.nodes.some((node) => node.local),
        );
        const hasVolumes = replicated.clusters.some(
          (cluster) => cluster.volumes.length > 0,
        );
        setEligible(clustered && !hasVolumes);
      } catch {
        // No card, and nothing lost: the pool view above still renders.
      }
    })();
  }, []);

  useEffect(() => {
    // Polling pauses while a dialog is open, so a refresh cannot move
    // what the operator is pointing at mid-choice.
    if (snapshotting || creating || destroying) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, snapshotting, creating, destroying]);

  // Keep the dialog's row current across reloads: it shows the snapshot
  // list, which its own actions change.
  useEffect(() => {
    if (!snapshotting || !pool) return;
    const fresh = pool.vdisks.find((v) => v.vdisk === snapshotting.vdisk);
    if (fresh && fresh !== snapshotting) setSnapshotting(fresh);
  }, [pool, snapshotting]);

  if (!pool && !broken && !error) {
    // No pool here. On a clustered node with no replicated volumes that is
    // an invitation, not an absence — the drive wizard's front door. Where
    // DRBD volumes exist the card does not render: a cluster runs one
    // engine, and the exclusivity is presented rather than just enforced.
    if (!eligible) return null;
    return (
      <section className="flex flex-col gap-2">
        <div className="callout">
          <div className="flex items-center gap-3 w-full">
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              <strong>Pooled storage</strong> — turn the cluster&apos;s data
              disks into one deduplicated pool, serving every machine disk on
              every member at once.
            </div>
            <span className="ml-auto">
              <Button kind="primary" size="sm" icon={Plus} onClick={() => setCreating(true)}>
                Create
              </Button>
            </span>
          </div>
        </div>
        {creating && (
          <CreateLumenPoolDialog
            onClose={() => setCreating(false)}
            onCreated={() => {
              setToast("The pool is up on every member.");
              setCreating(false);
              void load();
            }}
          />
        )}
      </section>
    );
  }

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
        {pool?.usable_bytes !== undefined && (
          <span
            className="text-[12px] text-[var(--qz-fg-3)]"
            title="From every member's space and its share of the 256 slices, before dedupe — dedupe only makes it bigger."
          >
            {formatBytes(pool.usable_bytes)} usable
          </span>
        )}
        {pool?.members.some(
          (m) => "Answered" in m.view && m.view.Answered.reassign_pending != null,
        ) && (
          <span
            className="badge badge-info"
            title="The pool is moving data to its new member layout; capacity and placement settle when it completes."
          >
            Rebalancing
          </span>
        )}
        {pool && (
          <span className="ml-auto" title="Destroy the pool: every brick wiped">
            <Button kind="ghost" size="sm" icon={Trash2} onClick={() => setDestroying(true)} />
          </span>
        )}
      </div>

      {(error ?? broken) && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error ?? broken}</div>
          {broken && !error && (
            <span className="ml-auto">
              <button
                type="button"
                className="btn btn-danger btn-sm"
                onClick={() => setDestroying(true)}
              >
                Tear down the broken pool
              </button>
            </span>
          )}
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

      {destroying && (
        <DestroyLumenPoolDialog
          vdisks={pool?.vdisks.length ?? 0}
          broken={broken}
          onClose={() => setDestroying(false)}
          onDestroyed={() => {
            setToast("The pool is gone; every brick was wiped.");
            setDestroying(false);
            void load();
          }}
        />
      )}
    </section>
  );
}

/// Destroying the pool: refused server-side while any machine disk exists
/// or any member is silent; allowed on a broken deployment, because that
/// is the way out of Broken. The ending mirrors the create's — the
/// coordinator restarts itself, the feed dies, and the observed absence
/// finishes the story.
function DestroyLumenPoolDialog({
  vdisks,
  broken,
  onClose,
  onDestroyed,
}: {
  vdisks: number;
  broken: string | null;
  onClose: () => void;
  onDestroyed: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);
  const [progress, setProgress] = useState<PoolProgress | null>(null);
  const [adopting, setAdopting] = useState(false);
  const [done, setDone] = useState(false);

  const submit = async () => {
    setBusy(true);
    setFailed(null);
    try {
      setProgress(await destroyLumenPool());
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setFailed(err instanceof Error ? err.message : "The destroy was refused.");
      setBusy(false);
    }
  };

  const running = progress?.phase === "running" && !adopting;
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => {
      void (async () => {
        try {
          const fresh = await fetchPoolPending();
          setProgress(fresh);
          if (fresh.phase === "complete") setAdopting(true);
          if (fresh.phase === "failed") setBusy(false);
        } catch {
          // The coordinator's own restart takes the feed with it.
          setAdopting(true);
        }
      })();
    }, 1000);
    return () => clearInterval(timer);
  }, [running]);
  useEffect(() => {
    if (!adopting || done) return;
    const timer = setInterval(() => {
      void (async () => {
        try {
          const response = await fetchPooledStorage();
          if (!response.pool && !response.error) {
            setDone(true);
            setBusy(false);
            onDestroyed();
          }
        } catch {
          // Still restarting.
        }
      })();
    }, 1000);
    return () => clearInterval(timer);
  }, [adopting, done, onDestroyed]);

  const guard = busy ? () => {} : onClose;
  return (
    <ModalShell onClose={guard} maxWidth={560}>
      <ModalHeader
        title="Destroy the pool"
        subtitle="Every brick is wiped, every member's daemon stops, and the disks return to the free list."
        onClose={guard}
      />
      <div className="flex flex-col gap-4">
        {!progress && (
          <>
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              {broken
                ? "This pool's configuration exists but cannot be assembled. Tearing it down is the way out: whatever the drop-in names is cleared, and the node stops claiming a pool."
                : vdisks > 0
                  ? `The pool still holds ${vdisks} disk${vdisks === 1 ? "" : "s"}. The destroy will be refused until the machines that use them are deleted — this is not a shortcut past them.`
                  : "The pool holds no machine disks, so nothing is lost but the pool itself."}
            </div>
            {failed && <div className="callout callout-crit">{failed}</div>}
            <CheckRow checked={acked} onChange={() => setAcked(!acked)}>
              I understand this may lose data.
            </CheckRow>
            <div className="flex justify-end gap-2">
              <button type="button" className="btn btn-ghost btn-sm" onClick={onClose}>
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-danger btn-sm"
                disabled={!acked || busy}
                onClick={() => void submit()}
              >
                {busy ? "Destroying…" : "Destroy the pool"}
              </button>
            </div>
          </>
        )}
        {progress && (
          <>
            {progress.phase === "failed" && (
              <div className="callout callout-crit">
                {progress.error ?? "The destroy failed."}
              </div>
            )}
            <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
              {progress.steps.map((step, index) => (
                <ProgressRow key={`${step.step}-${step.node ?? ""}-${index}`} step={step} />
              ))}
            </ul>
            {adopting && !done && (
              <div className="text-[13px] text-[var(--qz-fg-3)]">
                The control plane is restarting without the pool…
              </div>
            )}
            {(done || progress.phase === "failed") && (
              <div className="flex justify-end">
                <button type="button" className="btn btn-primary" onClick={onClose}>
                  Close
                </button>
              </div>
            )}
          </>
        )}
      </div>
    </ModalShell>
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
      // Bytes, now that the members say them; the segment fallback covers
      // a member mid-update that does not yet.
      if (status.usable_bytes > 0) {
        const percent = Math.round((status.free_bytes * 100) / status.usable_bytes);
        return (
          <span className={`qz-mono ${percent < 15 ? "text-[var(--qz-danger)]" : ""}`}>
            {formatBytes(status.free_bytes)} of {formatBytes(status.usable_bytes)}
          </span>
        );
      }
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

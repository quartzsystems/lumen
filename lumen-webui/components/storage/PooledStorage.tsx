"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Camera, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { ProgressRow } from "@/components/cluster/CreateClusterDialog";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { CreateLumenPoolDialog } from "@/components/storage/CreateLumenPoolDialog";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { CheckRow } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { fetchEnvironment } from "@/lib/clusterClient";
import {
  deletePoolDisk,
  deletePoolSnapshot,
  destroyLumenPool,
  fetchPooledStorage,
  fetchPoolPending,
  POOL_HEALTH_LABEL,
  POOL_HEALTH_TONE,
  replicationLabel,
  rollbackPoolDisk,
  scrubPool,
  takePoolSnapshot,
  type PooledStorageView,
  type PoolMember,
  type PoolProgress,
  type PoolVdisk,
} from "@/lib/poolClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 5000;

/// The pool, read once and kept current.
///
/// Two sections read the same answer from two different pages — the pool
/// itself lives under Pools, and the disks it serves live under Volumes — so
/// the reading is here and each page holds its own copy. One extra request per
/// open, and neither page can be made to wait on the other's state.
///
/// `paused` stops the poll while a dialog is open, so a refresh cannot move
/// what the operator is pointing at mid-choice.
function usePooledStorage(paused: boolean) {
  const [pool, setPool] = useState<PooledStorageView | null>(null);
  const [broken, setBroken] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  /// Whether the first answer has arrived. Until it has, the sections
  /// render nothing — "still loading" must never look like "no pool",
  /// or every page open starts with an invitation to create one.
  const [loaded, setLoaded] = useState(false);

  const load = useCallback(async () => {
    try {
      const response = await fetchPooledStorage();
      setPool(response.pool);
      setBroken(response.error);
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the pool.");
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (paused) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, paused]);

  return { pool, broken, error, loaded, load };
}

/// The LumenFS pool: pooled, deduplicated storage owned by the cluster,
/// serving each machine disk as the same `/dev/ublkb<id>` on every member.
///
/// This is the pool itself — its members, its replication, and the two things
/// that can be done to the whole of it. The disks it holds are volumes and are
/// listed with the other volumes, on their own page: a machine disk is the
/// same kind of object whether the pool serving it is this one or a node's own
/// ZFS, and an operator looking for one should not have to know which.
///
/// The section owns its own data and renders nothing at all on a
/// standalone node — absent rather than empty. The one exception is a pool
/// configuration that exists but could not be assembled: that is a broken
/// deployment, and it renders as its own sentence, because "nothing to
/// show" and "your deployment is broken" must never look alike.
export function PooledStorageSection() {
  const { setToast } = useConsole();
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState(false);
  /// Whether the empty-state create card may render: this node is in a
  /// cluster, because the pool spans one.
  const [eligible, setEligible] = useState(false);
  const { pool, broken, error, loaded, load } = usePooledStorage(creating || destroying);

  // Read once: whether a create is even offerable here. The fact changes
  // only through workflows that reload this page anyway.
  useEffect(() => {
    void (async () => {
      try {
        const environment = await fetchEnvironment();
        setEligible(
          environment.clusters.some((cluster) =>
            cluster.nodes.some((node) => node.local),
          ),
        );
      } catch {
        // No card, and nothing lost: the pool view above still renders.
      }
    })();
  }, []);

  if (!pool && !broken && !error) {
    // No pool here. On a clustered node that is an invitation, not an
    // absence — the drive wizard's front door. But only once the server
    // has actually said so: before the first answer this knows nothing,
    // and an invitation would read as the pool having vanished.
    if (!eligible || !loaded) return null;
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
          <span className="ml-auto inline-flex items-center gap-1">
            <span title="Scrub: verify every stored block against its address, on every member. Runs in the background; progress shows on each member's row.">
              <Button
                kind="ghost"
                size="sm"
                icon={ShieldCheck}
                disabled={pool.members.some(
                  (member) => "Answered" in member.view && member.view.Answered.scrub != null,
                )}
                onClick={() =>
                  void (async () => {
                    try {
                      const answer = await scrubPool();
                      setToast(`Scrub started on ${answer.started.join(", ")}.`);
                      await load();
                    } catch (err) {
                      setToast(
                        err instanceof Error ? err.message : "The scrub could not start.",
                      );
                    }
                  })()
                }
              />
            </span>
            <span title="Destroy the pool: every brick wiped">
              <Button kind="ghost" size="sm" icon={Trash2} onClick={() => setDestroying(true)} />
            </span>
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

      {pool && pool.health !== "Healthy" && (
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="flex flex-col gap-1 text-[13px] text-[var(--qz-fg-2)]">
            {pool.members
              .map((member) => diagnosis(member))
              .filter((sentence): sentence is string => sentence !== null)
              .map((sentence) => (
                <div key={sentence}>{sentence}</div>
              ))}
          </div>
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
          <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
            The {pool.vdisks.length} disk{pool.vdisks.length === 1 ? "" : "s"} this pool serves are
            under Storage → Volumes, with their snapshots.
          </p>
        </>
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

/// The disks the pool serves — the volumes half of the same answer.
///
/// On the Volumes page beside the nodes' own ZFS volumes, because a machine
/// disk is a machine disk: which engine serves it is a property of the disk,
/// not a different kind of object. Renders nothing when there is no pool, and
/// says nothing about the pool's health — that is next door, under Pools,
/// where the thing it is about lives.
export function PooledVolumesSection() {
  const { setToast } = useConsole();
  const [snapshotting, setSnapshotting] = useState<PoolVdisk | null>(null);
  /// The orphaned disk being reaped, when one is.
  const [reaping, setReaping] = useState<PoolVdisk | null>(null);
  const { pool, load } = usePooledStorage(snapshotting !== null || reaping !== null);

  // Keep the dialog's row current across reloads: it shows the snapshot
  // list, which its own actions change.
  useEffect(() => {
    if (!snapshotting || !pool) return;
    const fresh = pool.vdisks.find((v) => v.vdisk === snapshotting.vdisk);
    if (fresh && fresh !== snapshotting) setSnapshotting(fresh);
  }, [pool, snapshotting]);

  if (!pool) return null;

  return (
    <section className="flex flex-col gap-2">
      <div className="flex items-center gap-3">
        <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
          Pooled — {pool.name}
        </h2>
        <span
          className="text-[12px] text-[var(--qz-fg-3)]"
          title="Served as the same device on every member, so a machine using one can start anywhere."
        >
          {pool.vdisks.length} {pool.vdisks.length === 1 ? "disk" : "disks"}
        </span>
      </div>

      <DataTable
        rows={pool.vdisks}
        columns={vdiskColumns(pool)}
        rowId={(vdisk) => String(vdisk.vdisk)}
        storageKey="volumes-pooled"
        searchPlaceholder="Search disks…"
        emptyMessage="The pool holds no disks yet — they are created with a machine."
        onRefresh={load}
        actions={(vdisk) => (
          <div className="flex items-center gap-1 justify-end">
            <span title="Snapshots: instant, crash-consistent, on both members at once">
              <Button kind="ghost" size="sm" icon={Camera} onClick={() => setSnapshotting(vdisk)} />
            </span>
            <span
              title={
                vdisk.exported_on.length > 0
                  ? "Served right now — stop the machine using it first."
                  : vdisk.disk === null
                    ? "Not a machine disk; the pool's own bookkeeping."
                    : `Delete ${diskLabel(vdisk)} — for a volume its machine left behind.`
              }
            >
              <Button
                kind="ghost"
                size="sm"
                icon={Trash2}
                disabled={vdisk.exported_on.length > 0 || vdisk.disk === null}
                onClick={() => setReaping(vdisk)}
              />
            </span>
          </div>
        )}
      />

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

      {reaping && (
        <ReapDiskDialog
          vdisk={reaping}
          onClose={() => setReaping(null)}
          onReaped={() => {
            setToast(`${diskLabel(reaping)} deleted on every member.`);
            setReaping(null);
            void load();
          }}
        />
      )}
    </section>
  );
}

/// Reaping one orphaned volume: the disk a deleted machine left behind.
/// The backend refuses while any defined machine still names the device,
/// so the acknowledgement here is about the data, not about safety races.
function ReapDiskDialog({
  vdisk,
  onClose,
  onReaped,
}: {
  vdisk: PoolVdisk;
  onClose: () => void;
  onReaped: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);

  const submit = async () => {
    setBusy(true);
    setFailed(null);
    try {
      await deletePoolDisk(diskLabel(vdisk));
      onReaped();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setFailed(err instanceof Error ? err.message : "The delete was refused.");
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={busy ? () => {} : onClose} maxWidth={520}>
      <ModalHeader
        title={`Delete ${diskLabel(vdisk)}?`}
        subtitle="The volume and its snapshots are removed on every member at once."
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          This is for a volume its machine left behind — one kept on purpose when the machine
          was removed. A disk a defined machine still uses is refused here; detach it from the
          machine instead.
        </p>
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
            {busy ? "Deleting…" : "Delete the disk"}
          </button>
        </div>
      </div>
    </ModalShell>
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

/// A state badge names a fact; an operator needs the sentence behind it —
/// what the state means, and what to do about it. `null` for a member
/// that needs no explaining.
const diagnosis = (member: PoolMember): string | null => {
  if ("Silent" in member.view) {
    return `${member.name} did not answer: ${member.view.Silent}. The view cannot tell whether its data is fine, so the verdict says "cannot tell" rather than guessing.`;
  }
  const replication = member.view.Answered.replication;
  if (replication === "Suspended") {
    return `${member.name} has no peer link and no fence verdict, so its writes refuse and its flushes park — guests stall rather than diverge. This is almost always the peer session: if the other member rebooted or lost power, the surviving side can sit on a dead connection without noticing and never redial. Restarting lumen-fsd on the member that still shows Synced re-establishes the link, and the pair resyncs on its own.`;
  }
  if (replication === "Degraded") {
    return `${member.name} is serving alone under a fence verdict, at a bumped era. Writes are single-copy until its peer returns and adopts the missed history — that starts by itself when the link comes back.`;
  }
  if (typeof replication === "object") {
    return replication.Resyncing.source
      ? `${member.name} is the resync source: still serving guests while it streams the peer back up to date.`
      : `${member.name} is receiving a resync: its state is being replaced, so it refuses writes until the stream lands.`;
  }
  return null;
};

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
      const scrub = member.view.Answered.scrub ?? null;
      return (
        <span className="inline-flex items-center gap-2">
          <span className={`badge badge-${tone}`} title={diagnosis(member) ?? undefined}>
            {label}
          </span>
          {scrub && (
            <span
              className="badge badge-info"
              title={`Verifying every stored block against its address: ${scrub[0].toLocaleString()} of ${scrub[1].toLocaleString()} records. Guest I/O keeps running; the pass takes the engine one slice at a time.`}
            >
              Scrubbing{" "}
              {scrub[1] > 0 ? Math.round((scrub[0] * 100) / scrub[1]) : 0}%
            </span>
          )}
        </span>
      );
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

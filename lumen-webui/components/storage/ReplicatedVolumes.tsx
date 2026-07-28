"use client";

import { useCallback, useEffect, useMemo, useState, type CSSProperties } from "react";
import { AlertTriangle, Camera, Expand, Plus, Trash2, Unplug } from "lucide-react";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  CheckList,
  CheckRow,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
} from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { fetchEnvironment, type EnvironmentResponse } from "@/lib/clusterClient";
import {
  createReplicatedVolume,
  deleteVolumeSnapshot,
  destroyReplicatedVolume,
  fetchReplicatedVolumes,
  fetchVolumeSnapshots,
  resizeReplicatedVolume,
  resolveSplitBrain,
  rollbackVolume,
  snapshotVolume,
  VOLUME_HEALTH_LABEL,
  VOLUME_HEALTH_TONE,
  type ReplicatedVolumesResponse,
  type ReplicatedVolumeView,
  type SnapshotInfo,
} from "@/lib/drbdClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 5000;
const GIB = 1024 * 1024 * 1024;

/// Replicated volumes, grouped by cluster: one DRBD resource over a thick
/// zvol on each member, byte-exact by construction. The section owns its own
/// data — it exists only when an environment with clusters does, and the
/// pools above it stay exactly what they were.
export function ReplicatedVolumesSection() {
  const { setToast } = useConsole();
  const [volumes, setVolumes] = useState<ReplicatedVolumesResponse | null>(null);
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState<ReplicatedVolumeView | null>(null);
  const [growing, setGrowing] = useState<ReplicatedVolumeView | null>(null);
  const [snapshotting, setSnapshotting] = useState<ReplicatedVolumeView | null>(null);
  const [resolving, setResolving] = useState<ReplicatedVolumeView | null>(null);

  const load = useCallback(async () => {
    try {
      const [replicated, env] = await Promise.all([
        fetchReplicatedVolumes(),
        fetchEnvironment(),
      ]);
      setVolumes(replicated);
      setEnvironment(env);
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the replicated volumes.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // Polling pauses while a dialog is open, so a refresh cannot move a
    // picker out from under the operator mid-choice.
    if (creating || destroying || growing || snapshotting || resolving) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, creating, destroying, growing, snapshotting, resolving]);

  const clusters = environment?.clusters ?? [];
  // Volumes exist only inside a cluster; a standalone node has nothing to
  // show and no reason to see the section.
  if (clusters.length === 0) return null;

  const rows: ReplicatedVolumeView[] =
    volumes?.clusters.flatMap((cluster) => cluster.volumes) ?? [];

  return (
    <section className="flex flex-col gap-2">
      <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">Replicated Volumes</h2>
      {error && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}
      <VolumeTable
        rows={rows}
        onRefresh={load}
        onCreate={() => setCreating(true)}
        onDestroy={setDestroying}
        onGrow={setGrowing}
        onSnapshots={setSnapshotting}
        onResolve={setResolving}
      />

      {creating && environment && (
        <CreateVolumeDialog
          environment={environment}
          onClose={() => setCreating(false)}
          onCreated={(volume) => {
            setCreating(false);
            setToast(
              `${volume.name} created on ${volume.replicas.map((r) => r.node).join(" and ")}.`,
            );
            void load();
          }}
        />
      )}

      {destroying && (
        <DestroyVolumeDialog
          volume={destroying}
          onClose={() => setDestroying(null)}
          onDestroyed={() => {
            setToast(`${destroying.name} destroyed — every replica is gone.`);
            setDestroying(null);
            void load();
          }}
        />
      )}

      {growing && (
        <GrowVolumeDialog
          volume={growing}
          onClose={() => setGrowing(null)}
          onGrown={() => {
            setToast(`${growing.name} grown.`);
            setGrowing(null);
            void load();
          }}
        />
      )}

      {snapshotting && (
        <SnapshotsDialog
          volume={snapshotting}
          onClose={() => setSnapshotting(null)}
          onRolledBack={() => {
            setToast(`${snapshotting.name} rolled back — peers resync from the source.`);
            setSnapshotting(null);
            void load();
          }}
        />
      )}

      {resolving && (
        <ResolveSplitBrainDialog
          volume={resolving}
          onClose={() => setResolving(null)}
          onResolved={() => {
            setToast(`${resolving.name} reconnected — the victim resyncs from the survivors.`);
            setResolving(null);
            void load();
          }}
        />
      )}
    </section>
  );
}

const columns: Column<ReplicatedVolumeView>[] = [
  {
    key: "name",
    header: "Name",
    value: (volume) => volume.name,
    sortable: true,
    width: 180,
    render: (volume) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {volume.name}
      </span>
    ),
  },
  {
    key: "cluster",
    header: "Cluster",
    value: (volume) => volume.cluster,
    sortable: true,
    width: 110,
    render: (volume) => <span className="qz-mono">{volume.cluster}</span>,
  },
  {
    key: "state",
    header: "Replication",
    value: (volume) => volume.health,
    sortable: true,
    width: 190,
    render: (volume) => (
      <span
        className="inline-flex items-center gap-2"
        title={volume.error ?? undefined}
      >
        <span className={`badge badge-${VOLUME_HEALTH_TONE[volume.health]}`}>
          {VOLUME_HEALTH_LABEL[volume.health]}
          {volume.health === "syncing" && volume.sync_percent !== undefined
            ? ` ${volume.sync_percent.toFixed(1)}%`
            : ""}
        </span>
        {volume.health === "syncing" && volume.sync_rate_kib_s !== undefined && (
          <span className="qz-mono text-[11px] text-[var(--qz-fg-4)]">
            {formatBytes(volume.sync_rate_kib_s * 1024)}/s
          </span>
        )}
      </span>
    ),
  },
  {
    key: "size",
    header: "Size",
    value: (volume) => volume.size_bytes,
    sortable: true,
    width: 110,
    render: (volume) => <span className="qz-mono">{formatBytes(volume.size_bytes)}</span>,
  },
  {
    key: "device",
    header: "Device",
    value: (volume) => volume.device,
    width: 120,
    render: (volume) => <span className="qz-mono">{volume.device}</span>,
  },
  {
    key: "replicas",
    header: "Replicas",
    value: (volume) => volume.replicas.length,
    width: 220,
    render: (volume) =>
      volume.replicas.length === 0 ? (
        <Dash />
      ) : (
        <span className="inline-flex items-center gap-3">
          {volume.replicas.map((replica) => {
            const tone =
              replica.disk_state === undefined
                ? "muted"
                : replica.disk_state === "UpToDate"
                  ? "ok"
                  : replica.disk_state === "Inconsistent" || replica.disk_state === "Outdated"
                    ? "warn"
                    : "crit";
            return (
              <span
                key={replica.node}
                className="inline-flex items-center gap-[5px]"
                title={`${replica.node} · ${replica.pool} — ${replica.disk_state ?? "state unknown from this node"}${replica.role ? ` (${replica.role})` : ""}`}
              >
                <span className={`state-dot-${tone}`} />
                <span className="qz-mono text-[12px] text-[var(--qz-fg-3)]">{replica.node}</span>
              </span>
            );
          })}
        </span>
      ),
  },
];

function VolumeTable({
  rows,
  onRefresh,
  onCreate,
  onDestroy,
  onGrow,
  onSnapshots,
  onResolve,
}: {
  rows: ReplicatedVolumeView[];
  onRefresh: () => Promise<void>;
  onCreate: () => void;
  onDestroy: (volume: ReplicatedVolumeView) => void;
  onGrow: (volume: ReplicatedVolumeView) => void;
  onSnapshots: (volume: ReplicatedVolumeView) => void;
  onResolve: (volume: ReplicatedVolumeView) => void;
}) {
  return (
    <DataTable
      rows={rows}
      columns={columns}
      rowId={(volume) => `${volume.cluster}/${volume.name}`}
      storageKey="storage-replicated"
      searchPlaceholder="Search volumes…"
      emptyMessage="No replicated volumes yet. One lives on 2–3 members of a cluster and survives losing a node."
      onRefresh={onRefresh}
      toolbar={
        <Button kind="primary" size="sm" icon={Plus} onClick={onCreate}>
          Create
        </Button>
      }
      actionsWidth={136}
      actions={(volume) => (
        <span className="inline-flex items-center gap-1">
          {/* StandAlone means the replicas stopped talking and each kept its
              own history — the guided recovery is the only way forward. */}
          {volume.health === "stand_alone" && (
            <span title="Split-brain: pick the side whose writes are discarded">
              <Button
                kind="ghost"
                size="sm"
                icon={Unplug}
                onClick={() => onResolve(volume)}
              />
            </span>
          )}
          <span title="Snapshots — point-in-time copies on every member">
            <Button kind="ghost" size="sm" icon={Camera} onClick={() => onSnapshots(volume)} />
          </span>
          <span title="Grow the volume — a replicated volume only grows">
            <Button kind="ghost" size="sm" icon={Expand} onClick={() => onGrow(volume)} />
          </span>
          <span title="Destroy every replica">
            <Button kind="ghost" size="sm" icon={Trash2} onClick={() => onDestroy(volume)} />
          </span>
        </span>
      )}
    />
  );
}

/// The create dialog: a cluster, a name, a size, and the replica seats —
/// which members, and which pool on each. A two-node cluster replicates to
/// both nodes; the choice exists only at three and above.
function CreateVolumeDialog({
  environment,
  onClose,
  onCreated,
}: {
  environment: EnvironmentResponse;
  onClose: () => void;
  onCreated: (volume: ReplicatedVolumeView) => void;
}) {
  const clusters = environment.clusters;
  const [cluster, setCluster] = useState(clusters[0]?.name ?? "");
  const [name, setName] = useState("");
  const [sizeGib, setSizeGib] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [pools, setPools] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const members = useMemo(
    () => clusters.find((c) => c.name === cluster)?.nodes.map((n) => n.node) ?? [],
    [clusters, cluster],
  );
  const twoNode = members.length === 2;

  // A two-node cluster has exactly one placement; make it, rather than
  // asking the operator to tick the only possible boxes.
  useEffect(() => {
    setSelected(twoNode ? members : []);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cluster, twoNode]);

  const toggle = (node: string) => {
    if (twoNode) return;
    setSelected((current) =>
      current.includes(node) ? current.filter((n) => n !== node) : [...current, node],
    );
  };

  const size = Math.floor(Number(sizeGib) * GIB);
  const ready =
    name.trim().length > 0 &&
    size > 0 &&
    selected.length >= 2 &&
    selected.length <= 3 &&
    selected.every((node) => (pools[node] ?? "").trim().length > 0);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const volume = await createReplicatedVolume({
        cluster,
        name: name.trim(),
        size_bytes: size,
        members: selected.map((node) => ({ node, pool: (pools[node] ?? "").trim() })),
      });
      onCreated(volume);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The volume could not be created.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title="Create Replicated Volume"
        subtitle="One synchronously replicated block device: every write lands on every replica before it is acknowledged."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        {clusters.length > 1 && (
          <Field label="Cluster" required>
            <SelectInput mono value={cluster} onChange={setCluster}>
              {clusters.map((c) => (
                <option key={c.name} value={c.name}>
                  {c.name}
                </option>
              ))}
            </SelectInput>
          </Field>
        )}
        <div className="grid grid-cols-2 gap-3">
          <Field label="Name" required htmlFor="volume-name">
            <TextInput
              id="volume-name"
              value={name}
              onChange={setName}
              mono
              autoFocus
              placeholder="vm-101-disk-0"
            />
          </Field>
          <Field
            label="Size (GiB)"
            required
            hint="Usable size. The backing devices are sized byte-exactly above it, metadata included."
          >
            <TextInput value={sizeGib} onChange={setSizeGib} mono inputMode="numeric" />
          </Field>
        </div>

        <Field
          label={twoNode ? "Replicas (both nodes)" : `Replicas (${selected.length} of 2–3)`}
          hint={
            twoNode
              ? "A two-node cluster replicates every volume to both nodes — there is nowhere else for a replica to live."
              : "2 or 3 members. Three replicas carry their own quorum: losing a majority suspends I/O rather than diverging."
          }
        >
          <CheckList>
            {members.map((node) => (
              <CheckRow
                key={node}
                checked={selected.includes(node)}
                onChange={() => toggle(node)}
              >
                <span className="qz-mono text-[13px] text-[var(--qz-fg-1)]">{node}</span>
                {selected.includes(node) && (
                  <span className="ml-auto w-[180px]" onClick={(e) => e.stopPropagation()}>
                    <TextInput
                      mono
                      value={pools[node] ?? ""}
                      onChange={(v) => setPools((p) => ({ ...p, [node]: v }))}
                      placeholder="pool"
                    />
                  </span>
                )}
              </CheckRow>
            ))}
          </CheckList>
        </Field>
        <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
          The backing zvol lives in the named pool on each member. A fresh volume is available
          immediately — both replicas start identical, so there is no initial sync to wait out.
        </p>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!ready}
          submitLabel="Create volume"
          savingLabel="Creating…"
          onSubmit={() => void create()}
        />
      </div>
    </ModalShell>
  );
}

/// Typed-name confirmation: destroying a replicated volume destroys every
/// replica of its data, on every member.
function DestroyVolumeDialog({
  volume,
  onClose,
  onDestroyed,
}: {
  volume: ReplicatedVolumeView;
  onClose: () => void;
  onDestroyed: () => void;
}) {
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmed = typed.trim() === volume.name && acked;

  const destroy = async () => {
    setBusy(true);
    setError(null);
    try {
      await destroyReplicatedVolume(volume.cluster, volume.name, true);
      onDestroyed();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not destroy the volume.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Destroy ${volume.name}`}
        subtitle="Every replica is destroyed on every member. Replication does not protect against this — that is what the confirmation is for."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand this destroys the data on all {volume.replicas.length} replicas.
          </span>
        </label>
        <Field label={`Type ${volume.name} to confirm`} htmlFor="destroy-volume-name">
          <TextInput
            id="destroy-volume-name"
            value={typed}
            onChange={setTyped}
            mono
            autoFocus
            placeholder={volume.name}
          />
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          submitLabel="Destroy volume"
          savingLabel="Destroying…"
          onSubmit={() => void destroy()}
        />
      </div>
    </ModalShell>
  );
}

/// Grow only: every backing zvol first, then the resource, then the record.
function GrowVolumeDialog({
  volume,
  onClose,
  onGrown,
}: {
  volume: ReplicatedVolumeView;
  onClose: () => void;
  onGrown: () => void;
}) {
  const [sizeGib, setSizeGib] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const size = Math.floor(Number(sizeGib) * GIB);
  const ready = size > volume.size_bytes;

  const grow = async () => {
    setBusy(true);
    setError(null);
    try {
      await resizeReplicatedVolume(volume.cluster, volume.name, size);
      onGrown();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The volume could not grow.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Grow ${volume.name}`}
        subtitle={`Currently ${formatBytes(volume.size_bytes)}. A replicated volume only grows — a shrunk disk under a running guest is data loss with extra steps.`}
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <Field
          label="New size (GiB)"
          required
          hint="Every member's backing device grows first, then the volume takes the new size."
        >
          <TextInput value={sizeGib} onChange={setSizeGib} mono autoFocus inputMode="numeric" />
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!ready}
          submitLabel="Grow volume"
          savingLabel="Growing…"
          onSubmit={() => void grow()}
        />
      </div>
    </ModalShell>
  );
}

const formatSnapshotTime = (created: number): string =>
  new Date(created * 1000).toLocaleString();

/// Snapshots of one volume: the list this node's own replica holds, taking a
/// new one (on every member, all-or-none), deleting one, and the guided
/// rollback. The list is deliberately this node's vantage — a snapshot set is
/// per-member, and pretending to a merged view would hide exactly the
/// asymmetry a rollback needs the operator to think about.
function SnapshotsDialog({
  volume,
  onClose,
  onRolledBack,
}: {
  volume: ReplicatedVolumeView;
  onClose: () => void;
  onRolledBack: () => void;
}) {
  const [snapshots, setSnapshots] = useState<SnapshotInfo[] | null>(null);
  const [newName, setNewName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The rollback confirmation, opened per-snapshot.
  const [rollingBack, setRollingBack] = useState<string | null>(null);
  const [source, setSource] = useState("");
  const [acked, setAcked] = useState(false);

  const load = useCallback(async () => {
    try {
      setSnapshots(await fetchVolumeSnapshots(volume.cluster, volume.name));
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the snapshots.");
    }
  }, [volume.cluster, volume.name]);

  useEffect(() => {
    void load();
  }, [load]);

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
      await snapshotVolume(volume.cluster, volume.name, newName.trim());
      setNewName("");
      await load();
    }, "The snapshot could not be taken.");

  const remove = (snapshot: string) =>
    run(async () => {
      await deleteVolumeSnapshot(volume.cluster, volume.name, snapshot);
      if (rollingBack === snapshot) setRollingBack(null);
      await load();
    }, "The snapshot could not be deleted.");

  const rollback = () =>
    run(async () => {
      await rollbackVolume(volume.cluster, volume.name, rollingBack ?? "", source);
      onRolledBack();
    }, "The rollback did not complete.");

  const nameOk = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(newName.trim());

  return (
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title={`Snapshots of ${volume.name}`}
        subtitle="Taken on every member at once, all-or-none. Rolling back rewinds the whole volume to one member's copy."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        <div className="flex items-end gap-2">
          <div className="flex-1">
            <Field label="New snapshot" htmlFor="snapshot-name">
              <TextInput
                id="snapshot-name"
                value={newName}
                onChange={setNewName}
                mono
                placeholder="pre-upgrade"
              />
            </Field>
          </div>
          <button
            type="button"
            className="btn btn-primary btn-sm mb-[2px]"
            disabled={busy || !nameOk}
            onClick={() => void take()}
          >
            {busy ? "Working…" : "Take snapshot"}
          </button>
        </div>

        {snapshots === null && !error && (
          <div className="text-[13px] text-[var(--qz-fg-4)]">Reading snapshots…</div>
        )}
        {snapshots !== null && snapshots.length === 0 && (
          <div className="text-[13px] text-[var(--qz-fg-4)]">
            No snapshots yet. One is a point-in-time copy of the volume on every member.
          </div>
        )}

        {snapshots !== null && snapshots.length > 0 && (
          <ul className="m-0 p-0 flex flex-col gap-2" style={{ listStyle: "none" }}>
            {snapshots.map((snapshot) => (
              <li key={snapshot.name} className="surface p-3 flex flex-col gap-3">
                <div className="flex items-center gap-3">
                  <span className="qz-mono text-[13px] text-[var(--qz-fg-1)] flex-1 truncate">
                    {snapshot.name}
                  </span>
                  <span className="text-[12px] text-[var(--qz-fg-4)]">
                    {formatSnapshotTime(snapshot.created)} · {formatBytes(snapshot.used)} held
                  </span>
                  <button
                    type="button"
                    className="btn btn-ghost btn-sm"
                    disabled={busy}
                    onClick={() =>
                      setRollingBack(rollingBack === snapshot.name ? null : snapshot.name)
                    }
                  >
                    Roll back…
                  </button>
                  <span title="Delete this snapshot on every member">
                    <Button
                      kind="ghost"
                      size="sm"
                      icon={Trash2}
                      onClick={() => void remove(snapshot.name)}
                    />
                  </span>
                </div>

                {rollingBack === snapshot.name && (
                  <div className="flex flex-col gap-3 border-t border-[var(--qz-border)] pt-3">
                    <p className="text-[12px] text-[var(--qz-fg-3)] m-0">
                      The whole volume rewinds to this snapshot as one member holds it, and every
                      later snapshot on that member goes with it. The machine using this volume
                      must be powered off — the rollback refuses while the device is open.
                    </p>
                    <Field
                      label="Source member"
                      hint="Whose copy of the snapshot becomes the one truth — the others resync from it."
                    >
                      <SelectInput mono value={source} onChange={setSource}>
                        <option value="">Choose…</option>
                        {volume.replicas.map((replica) => (
                          <option key={replica.node} value={replica.node}>
                            {replica.node}
                          </option>
                        ))}
                      </SelectInput>
                    </Field>
                    <label className="qz-check-row">
                      <input
                        type="checkbox"
                        className="qz-check"
                        checked={acked}
                        onChange={() => setAcked(!acked)}
                        style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
                      />
                      <span className="text-[13px] text-[var(--qz-fg-2)]">
                        I understand everything written since this snapshot is lost, on every
                        member.
                      </span>
                    </label>
                    <div className="flex justify-end">
                      <button
                        type="button"
                        className="btn btn-danger btn-sm"
                        disabled={busy || source === "" || !acked}
                        onClick={() => void rollback()}
                      >
                        {busy ? "Rolling back…" : "Roll back the volume"}
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

/// The guided split-brain recovery: name the victim, lose its divergent
/// writes, reconnect every side. The dialog exists because DRBD's refusal to
/// merge divergent histories is correct — someone has to decide which
/// history survives, and that someone is the operator.
function ResolveSplitBrainDialog({
  volume,
  onClose,
  onResolved,
}: {
  volume: ReplicatedVolumeView;
  onClose: () => void;
  onResolved: () => void;
}) {
  const [victim, setVictim] = useState("");
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmed = victim !== "" && typed.trim() === victim && acked;

  const resolve = async () => {
    setBusy(true);
    setError(null);
    try {
      await resolveSplitBrain(volume.cluster, volume.name, victim);
      onResolved();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The recovery did not complete.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title={`Resolve split-brain on ${volume.name}`}
        subtitle="The replicas stopped talking and each kept writing its own history. One side's writes must be discarded before they can reconnect."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          Before choosing: check which side ran the machine last, and which side's writes matter.
          The victim's divergent writes are gone for good — it resyncs from the survivors.
        </p>
        <Field
          label="Victim"
          required
          hint="The member whose writes are discarded."
        >
          <SelectInput mono value={victim} onChange={setVictim}>
            <option value="">Choose…</option>
            {volume.replicas.map((replica) => (
              <option key={replica.node} value={replica.node}>
                {replica.node}
              </option>
            ))}
          </SelectInput>
        </Field>
        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand the victim's unreplicated writes are discarded.
          </span>
        </label>
        {victim && (
          <Field label={`Type ${victim} to confirm`} htmlFor="split-brain-victim">
            <TextInput
              id="split-brain-victim"
              value={typed}
              onChange={setTyped}
              mono
              placeholder={victim}
            />
          </Field>
        )}
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          submitLabel="Discard the victim's writes and reconnect"
          savingLabel="Reconnecting…"
          onSubmit={() => void resolve()}
        />
      </div>
    </ModalShell>
  );
}

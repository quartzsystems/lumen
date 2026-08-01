import { apiFetch } from "@/lib/authClient";

/// The LumenFS pool: pooled, deduplicated storage across the cluster's
/// members, serving each machine disk as the same device path everywhere.
///
/// The shapes mirror `lumen-pool`'s state module exactly — the view is
/// assembled server-side from what the members actually said, and the page
/// renders it without inventing anything.

/// One member's replication state. Rust's enum arrives externally tagged:
/// three of the four are bare strings, the resync carries its direction.
export type PoolReplication =
  | "Suspended"
  | "Synced"
  | "Degraded"
  | { Resyncing: { source: boolean } };

export interface PoolMemberStatus {
  node: number;
  replication: PoolReplication;
  era: number;
  accepts_writes: boolean;
  segments_free: number;
  segments_total: number;
  /// This member's space in bytes, labelled usable — every segment outside
  /// the collection reserve at full-block density. Dedupe only makes it
  /// conservative.
  usable_bytes: number;
  free_bytes: number;
  /// The same figures per tier, ascending.
  tiers: { tier: number; usable_bytes: number; free_bytes: number }[];
  vdisks: [number, number][];
  leases: [number, PoolLease][];
  stream: [number, number, number];
  /// Per-peer stream counters, `(peer, sent, durable, applied)` — the
  /// mesh's honest form. Empty from a pre-mesh daemon.
  peers?: [number, number, number, number][];
  /// The committed slice map's version; absent when unplaced (the
  /// two-member legacy, where every member holds everything).
  map_version?: number | null;
  /// How many of the 256 slices this member homes.
  seats?: number | null;
  /// The version a rebalance is moving to, while one is running.
  reassign_pending?: number | null;
  /// A background scrub in flight on this member: `[verified, total]`
  /// records. Absent when none is running.
  scrub?: [number, number] | null;
}

export interface PoolLease {
  holder: number;
  era: number;
  handing_to: number | null;
}

/// A member answered, or it did not — and silence carries its reason.
export type PoolMemberView =
  | { Answered: PoolMemberStatus }
  | { Silent: string };

export interface PoolMember {
  name: string;
  view: PoolMemberView;
}

export interface PoolSnapshot {
  /// The console passes Unix seconds as the id, so it displays as the
  /// moment it is.
  snapshot: number;
  size_bytes: number;
}

export interface PoolVdisk {
  vdisk: number;
  size_bytes: number;
  disk: { vmid: number; index: number } | null;
  device: string;
  holder: number | null;
  handing_to: number | null;
  exported_on: string[];
  snapshots: PoolSnapshot[];
}

export type PoolHealth = "Healthy" | "Degraded" | "Unknown" | "None";

export interface PooledStorageView {
  name: string;
  members: PoolMember[];
  vdisks: PoolVdisk[];
  health: PoolHealth;
  /// The one capacity figure: per-tier minimum over the members, summed.
  /// Absent while any member is silent — half a pool would be a guess.
  usable_bytes?: number;
}

export interface PooledStorageResponse {
  /// `null` on a node with no pool — the section renders nothing at all.
  pool: PooledStorageView | null;
  /// A pool configuration that exists but could not be assembled: a broken
  /// deployment, shown as its own sentence rather than an empty page.
  error: string | null;
}

export const fetchPooledStorage = (): Promise<PooledStorageResponse> =>
  apiFetch<PooledStorageResponse>("/storage/pool");

/// The routes address a disk by the compute domain's name for it —
/// `vm-7-disk-3` — the same fact as the device path, without the slashes.
export const takePoolSnapshot = (disk: string, snapshot: number): Promise<void> =>
  apiFetch<void>(`/storage/pool/disks/${encodeURIComponent(disk)}/snapshots`, {
    method: "POST",
    body: JSON.stringify({ snapshot }),
  });

export const deletePoolSnapshot = (disk: string, snapshot: number): Promise<void> =>
  apiFetch<void>(
    `/storage/pool/disks/${encodeURIComponent(disk)}/snapshots/${snapshot}`,
    { method: "DELETE" },
  );

/// Verify every stored block against its address, on every member at
/// once. Answers with who started; each member's progress rides the pool
/// view it already reports, as a scrub field on its status.
export const scrubPool = (): Promise<{ started: string[] }> =>
  apiFetch<{ started: string[] }>("/storage/pool/scrub", {
    method: "POST",
    body: JSON.stringify({}),
  });

/// Reap a pooled volume nothing references — the disk kept when its
/// machine was deleted without a purge. The backend refuses while any
/// defined machine still names the device, so this cannot take a disk
/// out from under one.
export const deletePoolDisk = (disk: string): Promise<void> =>
  apiFetch<void>(`/storage/pool/disks/${encodeURIComponent(disk)}`, {
    method: "DELETE",
    body: JSON.stringify({ i_understand_this_may_lose_data: true }),
  });

export const rollbackPoolDisk = (disk: string, snapshot: number): Promise<void> =>
  apiFetch<void>(`/storage/pool/disks/${encodeURIComponent(disk)}/rollback`, {
    method: "POST",
    body: JSON.stringify({ snapshot, i_understand_this_may_lose_data: true }),
  });

// --- the create/destroy workflows -------------------------------------------

import type { StepProgress, WorkflowPhase } from "@/lib/clusterClient";

/// A pool job as `/storage/pool/pending` reports it — the cluster create's
/// step shape, so `ProgressRow` renders both.
export interface PoolProgress {
  action: "create" | "destroy";
  phase: WorkflowPhase;
  error?: string;
  steps: StepProgress[];
}

export interface LumenBrickChoice {
  /// The kernel name as the picker reported it (`sdb`); each member
  /// resolves its own to a stable path before anything is formatted.
  disk: string;
  tier: number;
}

export interface LumenBrickSeat {
  node: string;
  bricks: LumenBrickChoice[];
}

export const createLumenPool = (
  seats: LumenBrickSeat[],
): Promise<PoolProgress> =>
  apiFetch<PoolProgress>("/storage/pool", {
    method: "POST",
    body: JSON.stringify({
      seats,
      i_understand_this_erases_the_disks: true,
    }),
  });

export const destroyLumenPool = (): Promise<PoolProgress> =>
  apiFetch<PoolProgress>("/storage/pool", {
    method: "DELETE",
    body: JSON.stringify({ i_understand_this_may_lose_data: true }),
  });

/// 404 until a job has been started — and again after the coordinator's
/// own restart, when the observed pool takes over as the truth.
export const fetchPoolPending = (): Promise<PoolProgress> =>
  apiFetch<PoolProgress>("/storage/pool/pending");

// --- display helpers ---------------------------------------------------------

export const POOL_HEALTH_TONE: Record<PoolHealth, "ok" | "warn" | "crit" | "muted"> = {
  Healthy: "ok",
  Degraded: "warn",
  // Missing evidence outranks observed trouble: a silent member could be
  // carrying the worse case, so this is not painted as merely degraded.
  Unknown: "crit",
  None: "muted",
};

export const POOL_HEALTH_LABEL: Record<PoolHealth, string> = {
  Healthy: "Healthy",
  Degraded: "Degraded",
  Unknown: "Cannot tell",
  None: "No pool",
};

/// One word for a member's replication state, and whether it wants an
/// operator's eyes.
export const replicationLabel = (
  replication: PoolReplication,
): { label: string; tone: "ok" | "warn" | "crit" } => {
  if (replication === "Synced") return { label: "Synced", tone: "ok" };
  if (replication === "Degraded") return { label: "Degraded — serving alone", tone: "warn" };
  if (replication === "Suspended") return { label: "Suspended", tone: "crit" };
  return {
    label: replication.Resyncing.source ? "Resyncing — serving" : "Resyncing — receiving",
    tone: "warn",
  };
};

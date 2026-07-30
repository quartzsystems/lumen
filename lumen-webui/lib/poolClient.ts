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
  vdisks: [number, number][];
  leases: [number, PoolLease][];
  stream: [number, number, number];
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

export const rollbackPoolDisk = (disk: string, snapshot: number): Promise<void> =>
  apiFetch<void>(`/storage/pool/disks/${encodeURIComponent(disk)}/rollback`, {
    method: "POST",
    body: JSON.stringify({ snapshot, i_understand_this_may_lose_data: true }),
  });

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

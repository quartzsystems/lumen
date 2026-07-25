import { apiFetch } from "@/lib/authClient";

// Typed view of /api/storage, modelled on lib/networkClient.ts. Read-only at
// this stage: pools are created and removed from the node itself until the
// privileged executor lands. See docs/compute.md.

export type PoolHealth =
  | "online"
  | "degraded"
  | "faulted"
  | "offline"
  | "removed"
  | "unavail"
  | "unknown";

export type DatasetKind = "filesystem" | "volume" | "snapshot";

/// One row of the pool table. Sizes are bytes; `used_percent` is computed by
/// the backend so the bar and the number can never disagree.
export interface PoolView {
  name: string;
  health: PoolHealth;
  size: number;
  allocated: number;
  free: number;
  used_percent: number;
  fragmentation: number | null;
  dedup_ratio: number | null;
  read_only: boolean;
  /// Always false at this stage; the reason is supplied rather than left to
  /// the console to invent.
  destroyable: boolean;
  destroy_blocked_reason: string | null;
}

export interface NodePools {
  node: string;
  pools: PoolView[];
}

export interface PoolsResponse {
  nodes: NodePools[];
}

export interface VolumeView {
  name: string;
  kind: DatasetKind;
  used: number;
  available: number | null;
  referenced: number;
  volsize: number | null;
  volblocksize: number | null;
  mountpoint: string | null;
  /// Created by Lumen, and therefore something Lumen may remove.
  lumen_managed: boolean;
}

export interface VolumesResponse {
  node: string;
  pool: string;
  volumes: VolumeView[];
}

export const fetchPools = (): Promise<PoolsResponse> => apiFetch<PoolsResponse>("/storage/pools");

export const fetchVolumes = (pool: string): Promise<VolumesResponse> =>
  apiFetch<VolumesResponse>(`/storage/pools/${encodeURIComponent(pool)}/volumes`);

/// Which badge a pool's health wears.
export const HEALTH_TONE: Record<PoolHealth, "ok" | "warn" | "crit" | "muted"> = {
  online: "ok",
  degraded: "warn",
  faulted: "crit",
  offline: "warn",
  removed: "crit",
  unavail: "crit",
  unknown: "muted",
};

import { apiFetch } from "@/lib/authClient";

// Typed view of /api/storage/replicated, modelled on lib/clusterClient.ts:
// exported interfaces plus thin functions, no logic. Field names mirror
// lumen-drbd's serde output exactly.

/// The replication pill, derived server-side so the console and the API can
/// never disagree about what a volume's state reads as.
export type VolumeHealth =
  | "up_to_date"
  | "syncing"
  | "degraded"
  | "outdated"
  | "suspended_no_quorum"
  | "suspended"
  | "stand_alone"
  | "down"
  | "unknown";

export interface ReplicaView {
  node: string;
  pool: string;
  /// This is the node whose console answered.
  local: boolean;
  disk_state?: string;
  role?: string;
  connection?: string;
}

export interface ReplicatedVolumeView {
  name: string;
  cluster: string;
  /// The DRBD resource name, `<cluster>-<name>`.
  resource: string;
  /// The stable block device, identical on every member.
  device: string;
  size_bytes: number;
  zvol_bytes: number;
  port: number;
  minor: number;
  health: VolumeHealth;
  sync_percent?: number;
  sync_rate_kib_s?: number;
  replicas: ReplicaView[];
  /// Why the state could not be read, when it could not.
  error?: string;
}

export interface ClusterVolumes {
  cluster: string;
  volumes: ReplicatedVolumeView[];
}

/// Grouped by cluster — a volume is cluster-scoped, so the cluster is the
/// group.
export interface ReplicatedVolumesResponse {
  clusters: ClusterVolumes[];
}

export interface VolumeMemberCreate {
  node: string;
  pool: string;
}

export interface VolumeCreateRequest {
  cluster: string;
  name: string;
  size_bytes: number;
  members: VolumeMemberCreate[];
}

export const fetchReplicatedVolumes = (): Promise<ReplicatedVolumesResponse> =>
  apiFetch<ReplicatedVolumesResponse>("/storage/replicated");

export const createReplicatedVolume = (
  request: VolumeCreateRequest,
): Promise<ReplicatedVolumeView> =>
  apiFetch<ReplicatedVolumeView>("/storage/replicated", {
    method: "POST",
    body: JSON.stringify(request),
  });

export const destroyReplicatedVolume = (
  cluster: string,
  name: string,
  acknowledge: boolean,
): Promise<void> =>
  apiFetch<void>(
    `/storage/replicated/${encodeURIComponent(cluster)}/${encodeURIComponent(name)}`,
    {
      method: "DELETE",
      body: JSON.stringify({ i_understand_this_may_lose_data: acknowledge }),
    },
  );

export const resizeReplicatedVolume = (
  cluster: string,
  name: string,
  sizeBytes: number,
): Promise<void> =>
  apiFetch<void>(
    `/storage/replicated/${encodeURIComponent(cluster)}/${encodeURIComponent(name)}/resize`,
    {
      method: "POST",
      body: JSON.stringify({ size_bytes: sizeBytes }),
    },
  );

// --- display helpers ---------------------------------------------------------

export const VOLUME_HEALTH_TONE: Record<VolumeHealth, "ok" | "warn" | "crit" | "muted"> = {
  up_to_date: "ok",
  syncing: "warn",
  degraded: "warn",
  outdated: "warn",
  suspended_no_quorum: "crit",
  suspended: "crit",
  stand_alone: "crit",
  down: "muted",
  unknown: "muted",
};

export const VOLUME_HEALTH_LABEL: Record<VolumeHealth, string> = {
  up_to_date: "UpToDate",
  syncing: "Syncing",
  degraded: "Degraded",
  outdated: "Outdated",
  suspended_no_quorum: "Suspended — no quorum",
  suspended: "Suspended",
  stand_alone: "StandAlone",
  down: "Down",
  unknown: "Unknown",
};

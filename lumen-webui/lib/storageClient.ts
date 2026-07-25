import { apiFetch } from "@/lib/authClient";

// Typed view of /api/storage, modelled on lib/networkClient.ts. Field names
// mirror lumen-zfs's serde output exactly, so the wire format is the only
// contract between the two.

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
  /// False for the pool this appliance is installed on; the reason is supplied
  /// rather than left to the console to invent.
  destroyable: boolean;
  destroy_blocked_reason: string | null;
}

/// How a pool's disks are arranged, and therefore what it survives.
export type VdevKind = "stripe" | "mirror" | "raidz1" | "raidz2" | "raidz3";

export type Compression = "off" | "lz4" | "zstd";

/// One disk the node has, as a candidate for a pool.
///
/// `in_use` is the field this type exists for: `zpool create` destroys
/// whatever was on the disks it is given, and a picker that cannot tell the
/// empty ones from the one the appliance is running from is a picker that will
/// eventually be used to reformat the wrong disk.
export interface BlockDevice {
  name: string;
  /// The stable path a pool should be built on — `/dev/disk/by-id/…` when the
  /// node has one. `/dev/sdb` is whatever the kernel enumerated second this
  /// boot, and a pool built on it can come back pointing at a different disk.
  path: string;
  kernel_path: string;
  size: number;
  model?: string;
  serial?: string;
  rotational: boolean;
  removable: boolean;
  in_use: boolean;
  /// What is on it, in words — "mounted at /", "3 partitions".
  used_by?: string;
}

export interface DevicesResponse {
  node: string;
  devices: BlockDevice[];
  /// The pool this appliance is installed on, which is never destroyed here.
  root_pool?: string;
}

export interface PoolCreate {
  name: string;
  vdev: VdevKind;
  disks: string[];
  ashift?: number;
  compression?: Compression;
  autotrim?: boolean;
  i_understand_this_may_lose_data?: boolean;
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

/// One installation image in a pool's media library.
export interface IsoView {
  /// The pool it is on. Called `storage` because that is the word the picker
  /// uses, and the picker is the only thing that reads it.
  storage: string;
  name: string;
  size: number;
  /// The absolute path a machine's drive points at. Read-only to the console —
  /// a drive is asked for by storage and name, never by path.
  path: string;
}

/// One pool's library, whether or not it has anything in it yet.
export interface IsoStoreView {
  storage: string;
  path: string;
  /// Readable and writable by the control plane right now. False means the
  /// dataset may well exist — `reason` says which.
  ready: boolean;
  reason?: string;
}

export interface IsosResponse {
  node: string;
  stores: IsoStoreView[];
  images: IsoView[];
}

export const fetchPools = (): Promise<PoolsResponse> => apiFetch<PoolsResponse>("/storage/pools");

/// Every disk the node has, and what is already on each. Read only when the
/// create dialog opens — scanning `/sys/block` is work the pool table should
/// not pay for.
export const fetchDevices = (): Promise<DevicesResponse> =>
  apiFetch<DevicesResponse>("/storage/devices");

export const createPool = (body: PoolCreate): Promise<PoolView> =>
  apiFetch<PoolView>("/storage/pools", { method: "POST", body: JSON.stringify(body) });

export const destroyPool = (pool: string, acknowledge: boolean): Promise<unknown> =>
  apiFetch(`/storage/pools/${encodeURIComponent(pool)}`, {
    method: "DELETE",
    body: JSON.stringify({ i_understand_this_may_lose_data: acknowledge }),
  });

/// What an arrangement costs and what it survives, computed the same way the
/// backend does so the dialog's estimate and the pool's real size are not two
/// different stories.
export const VDEV_INFO: Record<
  VdevKind,
  { label: string; minDisks: number; parity: number; note: string }
> = {
  stripe: {
    label: "Stripe",
    minDisks: 1,
    parity: 0,
    note: "Every disk's capacity, and no redundancy at all — one disk failing takes the pool with it.",
  },
  mirror: {
    label: "Mirror",
    minDisks: 2,
    parity: 1,
    note: "Every disk holds the same data. Survives all but one disk failing.",
  },
  raidz1: {
    label: "RAID-Z1",
    minDisks: 3,
    parity: 1,
    note: "One disk of parity. Survives one disk failing.",
  },
  raidz2: {
    label: "RAID-Z2",
    minDisks: 4,
    parity: 2,
    note: "Two disks of parity. Survives two disks failing.",
  },
  raidz3: {
    label: "RAID-Z3",
    minDisks: 5,
    parity: 3,
    note: "Three disks of parity. Survives three disks failing.",
  },
};

/// Bytes an arrangement leaves for data, before ZFS's own overhead.
export const usableBytes = (vdev: VdevKind, disks: number, smallest: number): number => {
  if (disks === 0) return 0;
  if (vdev === "mirror") return smallest;
  return smallest * Math.max(0, disks - VDEV_INFO[vdev].parity);
};

export const fetchIsos = (): Promise<IsosResponse> => apiFetch<IsosResponse>("/storage/iso");

export const createIsoStore = (pool: string): Promise<IsoStoreView> =>
  apiFetch<IsoStoreView>(`/storage/iso/${encodeURIComponent(pool)}`, { method: "POST" });

export const deleteIso = (pool: string, name: string): Promise<unknown> =>
  apiFetch(`/storage/iso/${encodeURIComponent(pool)}/${encodeURIComponent(name)}`, {
    method: "DELETE",
  });

/// Send one image to the node.
///
/// `XMLHttpRequest` rather than `fetch`: an installation image takes minutes to
/// upload and the only way to report progress while it does is upload
/// progress events, which `fetch` still does not have. The body is the file
/// itself — see the note on the endpoint for why it is not multipart.
export function uploadIso(
  pool: string,
  file: File,
  onProgress?: (fraction: number) => void,
): { promise: Promise<void>; abort: () => void } {
  const request = new XMLHttpRequest();
  const promise = new Promise<void>((resolve, reject) => {
    request.upload.addEventListener("progress", (event) => {
      if (event.lengthComputable) onProgress?.(event.loaded / event.total);
    });
    request.addEventListener("load", () => {
      if (request.status >= 200 && request.status < 300) {
        resolve();
        return;
      }
      // The API's own words where it gave any — "already in the library" is a
      // far better message than "Request failed (409)".
      let message = `Upload failed (${request.status})`;
      try {
        const body = JSON.parse(request.responseText);
        if (body?.error) message = body.error;
      } catch {}
      reject(new Error(message));
    });
    request.addEventListener("error", () => reject(new Error("Could not reach the server.")));
    request.addEventListener("abort", () => reject(new Error("Upload cancelled.")));
  });

  const url = `/api/storage/iso/${encodeURIComponent(pool)}/${encodeURIComponent(file.name)}`;
  request.open("PUT", url);
  request.withCredentials = true;
  request.setRequestHeader("Content-Type", "application/octet-stream");
  request.send(file);

  return { promise, abort: () => request.abort() };
}

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

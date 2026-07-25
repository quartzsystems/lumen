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

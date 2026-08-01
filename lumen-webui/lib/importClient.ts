import { apiFetch } from "@/lib/authClient";
import type { StepProgress, WorkflowPhase } from "@/lib/clusterClient";
import type { DiskBus, Firmware, NicModel, ScsiController } from "@/lib/vmClient";

/// Importing a machine from a VMware archive: upload the .ova, read back the
/// machine it describes, commit the corrected proposal, watch the job.

/// One disk inside the archive: what the guest sees, and how big the
/// compressed stream actually is.
export interface OvaDisk {
  file: string;
  /// Bytes the guest sees — the size the volume is created at.
  capacity: number;
  /// Bytes in the archive.
  size: number;
  /// The bus the source machine had it on, in Lumen's spelling.
  bus: DiskBus;
}

/// One adapter, still naming the source's network — which bridge that maps
/// to is exactly what the dialog asks.
export interface OvaNic {
  network: string;
  model: NicModel;
  mac?: string;
}

/// The machine one archive describes, as the backend proposes it.
export interface OvaAppliance {
  name: string;
  vcpus: number;
  memory_mib: number;
  firmware: Firmware;
  scsi_controller?: ScsiController | null;
  os?: string;
  disks: OvaDisk[];
  nics: OvaNic[];
  /// Hardware that was read but could not be carried, one sentence each.
  warnings: string[];
}

/// One archive waiting in the spool.
export interface ImportView {
  name: string;
  size: number;
  appliance?: OvaAppliance;
  error?: string;
}

/// The import job as the console polls it — the same step vocabulary the
/// pool and cluster workflows use.
export interface ImportProgress {
  source: string;
  vmid?: number;
  phase: WorkflowPhase;
  error?: string;
  steps: StepProgress[];
}

/// Where one archive disk lands.
export interface ImportDiskTarget {
  pool?: string;
  replicated?: boolean;
  bus?: DiskBus;
  discard?: boolean;
}

/// What one source adapter becomes. No bridge leaves the adapter behind.
export interface ImportNicTarget {
  bridge?: string;
  model?: NicModel;
  vlan_tag?: number;
  mac?: string;
}

/// POST /api/vms/import/{name}. Everything the archive already states is
/// optional and defaults to what it said; the required parts are what the
/// archive cannot know — where things land here.
export interface VmImportCommit {
  name: string;
  vmid?: number;
  description?: string;
  vcpus?: number;
  memory_mib?: number;
  firmware?: Firmware;
  disks: ImportDiskTarget[];
  nics: ImportNicTarget[];
  start?: boolean;
  keep_source?: boolean;
}

export const fetchImports = (): Promise<{ imports: ImportView[] }> =>
  apiFetch<{ imports: ImportView[] }>("/vms/import");

export const deleteImport = (name: string): Promise<unknown> =>
  apiFetch(`/vms/import/${encodeURIComponent(name)}`, { method: "DELETE" });

export const commitImport = (name: string, body: VmImportCommit): Promise<ImportProgress> =>
  apiFetch<ImportProgress>(`/vms/import/${encodeURIComponent(name)}`, {
    method: "POST",
    body: JSON.stringify(body),
  });

export const fetchImportPending = (): Promise<ImportProgress> =>
  apiFetch<ImportProgress>("/vms/import/pending");

/// Send one archive to the node — XMLHttpRequest for the same reason
/// `uploadIso` uses it: an archive takes minutes and only upload progress
/// events can say how it is going. Unlike an image upload the answer
/// matters: it is the machine the archive describes.
export function uploadOva(
  file: File,
  onProgress?: (fraction: number) => void,
): { promise: Promise<{ name: string; size: number; appliance: OvaAppliance }>; abort: () => void } {
  const request = new XMLHttpRequest();
  const promise = new Promise<{ name: string; size: number; appliance: OvaAppliance }>(
    (resolve, reject) => {
      request.upload.addEventListener("progress", (event) => {
        if (event.lengthComputable) onProgress?.(event.loaded / event.total);
      });
      request.addEventListener("load", () => {
        if (request.status >= 200 && request.status < 300) {
          try {
            resolve(JSON.parse(request.responseText));
            return;
          } catch {
            reject(new Error("The server's answer was unreadable."));
            return;
          }
        }
        // The API's own words where it gave any — "contains 2 machines" is
        // a far better message than "Request failed (409)".
        let message = `Upload failed (${request.status})`;
        try {
          const body = JSON.parse(request.responseText);
          if (body?.error) message = body.error;
        } catch {}
        reject(new Error(message));
      });
      request.addEventListener("error", () => reject(new Error("Could not reach the server.")));
      request.addEventListener("abort", () => reject(new Error("Upload cancelled.")));
    },
  );

  const url = `/api/vms/import/${encodeURIComponent(file.name)}`;
  request.open("PUT", url);
  request.withCredentials = true;
  request.setRequestHeader("Content-Type", "application/octet-stream");
  request.send(file);

  return { promise, abort: () => request.abort() };
}

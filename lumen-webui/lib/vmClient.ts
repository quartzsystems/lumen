import { apiFetch } from "@/lib/authClient";

// Typed view of /api/vms, modelled on lib/networkClient.ts: exported
// interfaces plus thin functions, no logic. Field names mirror lumen-virt's
// serde output exactly, so the wire format is the only contract between the
// two.

export type DomainState = "running" | "paused" | "shut_off" | "crashed" | "suspended" | "unknown";
export type Firmware = "uefi" | "bios";
export type BootDevice = "disk" | "cdrom" | "network";
export type DiskBus = "virtio-blk" | "virtio-scsi" | "sata";
export type CacheMode = "none" | "writeback" | "writethrough" | "directsync" | "unsafe";
export type NicModel = "virtio" | "e1000e" | "rtl8139";

/// How the guest sees the host processor. The two host-derived modes are bare
/// strings; a named model carries the name, which is how serde spells an enum
/// variant that has a value.
export type CpuModel = "host_model" | "host_passthrough" | { named: string };

export interface CpuTopology {
  sockets: number;
  cores: number;
  threads: number;
}

export interface VmDisk {
  /// The target name — `vda`, `sdb`. What the API addresses a disk by.
  id: string;
  bus: DiskBus;
  source: string;
  size: number;
  cache: CacheMode;
  discard: boolean;
  boot_index?: number | null;
}

export interface VmNic {
  /// The hardware address, which is also the handle a detach uses.
  id: string;
  model: NicModel;
  bridge: string;
  vlan_tag?: number | null;
  boot_index?: number | null;
}

/// Whether a lifecycle control is available, and why not when it is not — so
/// a disabled control explains itself instead of being silently grey.
export interface Action {
  allowed: boolean;
  reason?: string;
  /// The dialog must collect the acknowledgement before sending this one.
  requires_acknowledgement: boolean;
}

export interface VmActions {
  start: Action;
  shutdown: Action;
  stop: Action;
  reboot: Action;
  reset: Action;
  delete: Action;
}

/// One row of the machine table, and the whole of the detail page. Everything
/// either needs is here — neither makes a second request to fill a column.
export interface VmView {
  vmid: number;
  name: string;
  node: string;
  description: string | null;
  state: DomainState;
  tags: string[];

  vcpus: number;
  memory_mib: number;
  cpu_model: CpuModel;
  topology: CpuTopology | null;
  machine: string;
  firmware: Firmware;
  boot_order: BootDevice[];
  start_on_boot: boolean;
  guest_agent: boolean;
  disks: VmDisk[];
  nics: VmNic[];

  /// What the running machine is actually doing. All null when it is not.
  current_vcpus: number | null;
  current_memory_mib: number | null;
  cpu_time_ns: number | null;
  uptime_secs: number | null;

  boot_disk: string | null;
  total_disk_bytes: number;
  vnc_socket: string;
  /// Stored changes that will not reach the guest until it restarts.
  pending_reboot: string[];
  actions: VmActions;
}

export interface NodeVms {
  node: string;
  vms: VmView[];
}

/// Grouped by node from day one — one node today, the same shape tomorrow.
export interface VmsResponse {
  nodes: NodeVms[];
}

/// A rejected setting, tied to the machine and field it belongs to so a dialog
/// can render it against the offending input rather than in a banner.
export interface ValidationError {
  code: string;
  vm?: string;
  field?: string;
  message: string;
}

export interface VmUpdateResponse {
  vm: VmView;
  /// Changes the running machine took immediately.
  applied_live: string[];
  /// Changes stored but waiting for a restart, with the hypervisor's reason.
  pending_reboot: string[];
}

export interface VmDeleteResponse {
  vmid: number;
  name: string;
  removed_volumes: string[];
  kept_volumes: string[];
}

export interface DiskCreate {
  pool: string;
  size_gib: number;
  bus?: DiskBus;
  cache?: CacheMode;
  discard?: boolean;
  blocksize?: number;
}

export interface NicCreate {
  bridge: string;
  model?: NicModel;
  vlan_tag?: number;
}

export interface VmCreate {
  name: string;
  vmid?: number;
  description?: string;
  vcpus?: number;
  memory_mib?: number;
  cpu_model?: CpuModel;
  topology?: CpuTopology;
  machine?: string;
  firmware?: Firmware;
  boot_order?: BootDevice[];
  start_on_boot?: boolean;
  guest_agent?: boolean;
  tags?: string[];
  disks?: DiskCreate[];
  nics?: NicCreate[];
  /// Start it as soon as it is defined.
  start?: boolean;
}

export interface VmPatch {
  name?: string;
  description?: string;
  vcpus?: number;
  memory_mib?: number;
  cpu_model?: CpuModel;
  topology?: CpuTopology;
  machine?: string;
  firmware?: Firmware;
  boot_order?: BootDevice[];
  start_on_boot?: boolean;
  guest_agent?: boolean;
  tags?: string[];
}

const post = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "POST", body: JSON.stringify(body ?? {}) });

const patch = <T>(path: string, body: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "PATCH", body: JSON.stringify(body) });

const del = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "DELETE", body: JSON.stringify(body ?? {}) });

export const fetchVms = (): Promise<VmsResponse> => apiFetch<VmsResponse>("/vms");

export const fetchVm = (vmid: number): Promise<VmView> => apiFetch<VmView>(`/vms/${vmid}`);

export const createVm = (body: VmCreate): Promise<VmView> => post<VmView>("/vms", body);

export const updateVm = (vmid: number, body: VmPatch): Promise<VmUpdateResponse> =>
  patch<VmUpdateResponse>(`/vms/${vmid}`, body);

export const deleteVm = (
  vmid: number,
  purgeDisks: boolean,
  acknowledge: boolean,
): Promise<VmDeleteResponse> =>
  del<VmDeleteResponse>(`/vms/${vmid}`, {
    purge_disks: purgeDisks,
    i_understand_this_may_lose_data: acknowledge,
  });

/// The five lifecycle verbs. `stop` and `reset` carry the acknowledgement the
/// backend demands; the others take none, and sending one would be a 400.
export const startVm = (vmid: number): Promise<VmView> => post<VmView>(`/vms/${vmid}/start`);
export const shutdownVm = (vmid: number): Promise<VmView> => post<VmView>(`/vms/${vmid}/shutdown`);
export const rebootVm = (vmid: number): Promise<VmView> => post<VmView>(`/vms/${vmid}/reboot`);

export const stopVm = (vmid: number, acknowledge: boolean): Promise<VmView> =>
  post<VmView>(`/vms/${vmid}/stop`, { i_understand_this_may_lose_data: acknowledge });

export const resetVm = (vmid: number, acknowledge: boolean): Promise<VmView> =>
  post<VmView>(`/vms/${vmid}/reset`, { i_understand_this_may_lose_data: acknowledge });

export const attachDisk = (vmid: number, body: DiskCreate): Promise<VmUpdateResponse> =>
  post<VmUpdateResponse>(`/vms/${vmid}/disks`, body);

export const detachDisk = (
  vmid: number,
  id: string,
  purge: boolean,
  acknowledge: boolean,
): Promise<VmUpdateResponse> =>
  del<VmUpdateResponse>(`/vms/${vmid}/disks/${encodeURIComponent(id)}`, {
    purge_disks: purge,
    i_understand_this_may_lose_data: acknowledge,
  });

export const attachNic = (vmid: number, body: NicCreate): Promise<VmUpdateResponse> =>
  post<VmUpdateResponse>(`/vms/${vmid}/nics`, body);

export const detachNic = (vmid: number, id: string): Promise<VmUpdateResponse> =>
  del<VmUpdateResponse>(`/vms/${vmid}/nics/${encodeURIComponent(id)}`);

// --- display helpers ---------------------------------------------------------

/// How a state reads in the console, and which badge it wears.
export const STATE_LABEL: Record<DomainState, string> = {
  running: "running",
  paused: "paused",
  shut_off: "stopped",
  crashed: "crashed",
  suspended: "suspended",
  unknown: "unknown",
};

export const STATE_TONE: Record<DomainState, "ok" | "warn" | "crit" | "muted"> = {
  running: "ok",
  paused: "warn",
  shut_off: "muted",
  crashed: "crit",
  suspended: "warn",
  unknown: "muted",
};

/// The processor model as one word an operator can read.
export const cpuModelLabel = (model: CpuModel): string =>
  typeof model === "string"
    ? model === "host_model"
      ? "host model"
      : "host passthrough"
    : model.named;

/// Bytes as the size an operator wrote down. Binary units, because that is
/// what both the pool and the guest use.
export const formatBytes = (bytes: number): string => {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  const power = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** power;
  return `${value >= 10 || power === 0 ? Math.round(value) : value.toFixed(1)} ${units[power]}`;
};

export const formatMib = (mib: number): string => formatBytes(mib * 1024 * 1024);

/// Uptime as the coarsest unit that is still true — an operator glancing at a
/// table wants "3 d", not "3 d 4 h 12 m 9 s".
export const formatUptime = (seconds: number | null): string => {
  if (seconds === null) return "";
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h ${minutes % 60} m`;
  return `${Math.floor(hours / 24)} d ${hours % 24} h`;
};

/// Per-field validation errors off a rejected request. The backend answers a
/// rejected machine with the standard `{ error }` envelope plus an `errors`
/// array; `apiFetch` keeps the decoded body on the thrown ApiError.
export const validationErrorsOf = (err: unknown): ValidationError[] => {
  const body = (err as { body?: { errors?: unknown } } | null)?.body;
  const errors = body?.errors;
  return Array.isArray(errors) ? (errors as ValidationError[]) : [];
};

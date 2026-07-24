import { apiFetch } from "@/lib/authClient";

// Typed view of /api/network, modelled on lib/system.ts: exported interfaces
// plus thin functions, no logic. Field names mirror lumen-net's serde output
// exactly, so the wire format is the only contract between the two.

export type LinkKind = "ethernet" | "bond" | "bridge" | "vlan" | "other";
export type LinkState = "activated" | "activating" | "disconnected" | "unmanaged" | "unknown";
export type ChangeState = "unchanged" | "created" | "modified" | "deleted";
export type BondMode = "active-backup" | "802.3ad" | "balance-xor";
export type Duplex = "full" | "half";

/// One row of the interfaces table. Everything a row needs is here — the
/// table never makes a second request to fill a column.
export interface LinkView {
  name: string;
  kind: LinkKind;
  admin_up: boolean;
  oper_state: LinkState;
  carrier: boolean;
  perm_mac: string | null;
  mac: string | null;
  speed_mbps: number | null;
  duplex: Duplex | null;
  mtu: number | null;
  addresses: string[];
  gateway: string | null;
  dns: string[];
  controller: string | null;
  ports: string[];
  bond_mode: BondMode | null;
  vlan_id: number | null;
  parent: string | null;
  management: boolean;
  deletable: boolean;
  delete_blocked_reason: string | null;
  change: ChangeState;
  present: boolean;
}

export interface NodeInterfaces {
  node: string;
  interfaces: LinkView[];
}

/// Grouped by node from day one — one node today, the same shape tomorrow.
export interface InterfacesResponse {
  nodes: NodeInterfaces[];
}

/// A rejected setting, tied to the link and field it belongs to so the dialog
/// can render it against the offending input rather than in a banner.
export interface ValidationError {
  code: string;
  link?: string;
  field?: string;
  message: string;
}

export interface PendingChange {
  link: string;
  kind: LinkKind;
  change: ChangeState;
}

/// An applied change waiting to be confirmed. `confirm_deadline` is absolute
/// (epoch seconds) so a page reload or a slow request cannot drift the
/// countdown.
export interface CheckpointView {
  id: string;
  confirm_deadline: number;
  seconds_remaining: number;
  rollback_secs: number;
}

export interface PendingResponse {
  node: string;
  target: DesiredState | null;
  changes: PendingChange[];
  errors: ValidationError[];
  requires_disconnect_ack: boolean;
  checkpoint: CheckpointView | null;
}

export interface ApplyResponse {
  checkpoint: CheckpointView;
  operations: string[];
}

export interface ManagementBridgeResponse {
  bridge: string;
  converted: boolean;
  checkpoint: CheckpointView | null;
  operations: string[];
}

export type IpConfig =
  | { mode: "dhcp" }
  | { mode: "static"; cidr: string; gateway: string; dns: string[] }
  | { mode: "disabled" };

export interface DesiredState {
  nics: { name: string; mtu?: number; autoneg?: boolean; speed?: number; duplex?: Duplex }[];
  bonds: BondInput[];
  vlans: VlanInput[];
  bridges: BridgeInput[];
  management: { link: string; ip: IpConfig };
}

export interface BridgeInput {
  name: string;
  ports: string[];
  stp: boolean;
  forward_delay?: number;
  vlan_filtering: boolean;
  mac_address?: string;
  mtu?: number;
}

export interface BondInput {
  name: string;
  mode: BondMode;
  ports: string[];
  miimon?: number;
  lacp_rate?: "slow" | "fast";
  xmit_hash_policy?: "layer2" | "layer2+3" | "layer3+4";
  primary?: string;
  mtu?: number;
}

export interface VlanInput {
  name: string;
  parent: string;
  vlan_id: number;
  mtu?: number;
}

export interface NicPatch {
  mtu?: number;
  autoneg?: boolean;
  speed?: number;
  duplex?: Duplex;
}

const post = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "POST", body: JSON.stringify(body ?? {}) });

const patch = <T>(path: string, body: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "PATCH", body: JSON.stringify(body) });

const del = <T>(path: string): Promise<T> => apiFetch<T>(path, { method: "DELETE" });

export const fetchInterfaces = (): Promise<InterfacesResponse> =>
  apiFetch<InterfacesResponse>("/network/interfaces");

export const fetchPending = (): Promise<PendingResponse> =>
  apiFetch<PendingResponse>("/network/pending");

export const fetchConfig = (): Promise<DesiredState> => apiFetch<DesiredState>("/network/config");

export const createBridge = (bridge: BridgeInput): Promise<PendingResponse> =>
  post<PendingResponse>("/network/bridges", bridge);

export const createBond = (bond: BondInput): Promise<PendingResponse> =>
  post<PendingResponse>("/network/bonds", bond);

export const createVlan = (vlan: VlanInput): Promise<PendingResponse> =>
  post<PendingResponse>("/network/vlans", vlan);

export const updateBridge = (
  name: string,
  body: Partial<BridgeInput>,
): Promise<PendingResponse> => patch<PendingResponse>(`/network/bridges/${name}`, body);

export const updateBond = (name: string, body: Partial<BondInput>): Promise<PendingResponse> =>
  patch<PendingResponse>(`/network/bonds/${name}`, body);

export const updateVlan = (name: string, body: Partial<VlanInput>): Promise<PendingResponse> =>
  patch<PendingResponse>(`/network/vlans/${name}`, body);

export const updateNic = (name: string, body: NicPatch): Promise<PendingResponse> =>
  patch<PendingResponse>(`/network/nics/${name}`, body);

export const deleteLink = (kind: LinkKind, name: string): Promise<PendingResponse> =>
  del<PendingResponse>(`/network/${kind}s/${name}`);

export const discardPending = (): Promise<PendingResponse> =>
  del<PendingResponse>("/network/pending");

export const applyPending = (acknowledgeDisconnect: boolean): Promise<ApplyResponse> =>
  post<ApplyResponse>("/network/apply", {
    i_understand_this_may_disconnect_me: acknowledgeDisconnect,
  });

export const confirmApply = (): Promise<PendingResponse> => post<PendingResponse>("/network/confirm");

export const rollbackApply = (): Promise<PendingResponse> =>
  post<PendingResponse>("/network/rollback");

export const extendApply = (seconds: number): Promise<CheckpointView> =>
  post<CheckpointView>("/network/apply/extend", { seconds });

export const convertManagementBridge = (): Promise<ManagementBridgeResponse> =>
  post<ManagementBridgeResponse>("/network/management-bridge");

/// Per-field validation errors off a rejected request. The backend answers a
/// rejected configuration with the standard `{ error }` envelope plus an
/// `errors` array; `apiFetch` keeps the decoded body on the thrown ApiError.
export const validationErrorsOf = (err: unknown): ValidationError[] => {
  const body = (err as { body?: { errors?: unknown } } | null)?.body;
  const errors = body?.errors;
  return Array.isArray(errors) ? (errors as ValidationError[]) : [];
};

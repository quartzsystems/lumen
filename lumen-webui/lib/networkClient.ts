import { apiFetch } from "@/lib/authClient";

// Typed view of /api/network, modelled on lib/system.ts: exported interfaces
// plus thin functions, no logic. Field names mirror lumen-net's serde output
// exactly, so the wire format is the only contract between the two.

/// One nicN name and the card it is pinned to. A pin whose card is not in
/// the machine is what a replaced adapter leaves behind: every profile
/// naming it is bound to nothing until an operator says which new card
/// takes its place.
export interface NicPin {
  slot: number;
  mac: string;
  altname: string | null;
  present: boolean;
}

/// An adapter the node has that no name claims — a card added or swapped
/// in since the names were written.
export interface UnclaimedNic {
  device: string;
  mac: string;
  carrier: boolean;
  speed_mbps: number | null;
  driver: string | null;
}

export interface PinReport {
  orphaned: NicPin[];
  unclaimed: UnclaimedNic[];
}

export const fetchNicPins = (): Promise<PinReport> =>
  apiFetch<PinReport>("/network/nics/pins");

/// Give an orphaned name to a card that is actually in the machine. The
/// backend refuses a live slot or an absent adapter.
export const adoptNic = (
  slot: number,
  mac: string,
): Promise<{ adopted: string; device: string; active: boolean; note?: string }> =>
  apiFetch("/network/nics/adopt", {
    method: "POST",
    body: JSON.stringify({ slot, mac }),
  });

export type LinkKind = "ethernet" | "bond" | "bridge" | "vlan" | "other";
export type LinkState = "activated" | "activating" | "disconnected" | "unmanaged" | "unknown";
export type ChangeState = "unchanged" | "created" | "modified" | "deleted";
export type BondMode = "active-backup" | "802.3ad" | "balance-xor";
export type Duplex = "full" | "half";

/// One row of the interfaces table. Everything a row needs is here — the
/// table never makes a second request to fill a column.
export interface LinkView {
  name: string;
  /// What the kernel called this NIC before it was pinned to nicN, e.g.
  /// "enp3s0". Null for virtual links and NICs that were never renamed.
  altname: string | null;
  kind: LinkKind;
  admin_up: boolean;
  oper_state: LinkState;
  carrier: boolean;
  perm_mac: string | null;
  mac: string | null;
  speed_mbps: number | null;
  duplex: Duplex | null;
  mtu: number | null;
  /// What the box has on the link right now.
  addresses: string[];
  gateway: string | null;
  dns: string[];
  /// What the configuration asks for — this is what the dialog edits, and what
  /// a staged-but-unapplied row shows.
  ip: IpConfig;
  controller: string | null;
  ports: string[];
  bond_mode: BondMode | null;
  vlan_id: number | null;
  parent: string | null;
  /// A bridge that passes tagged frames through to its ports.
  vlan_aware: boolean;
  comment: string | null;
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

/// Addressing, as every link now carries it. `dns` is optional on the wire —
/// the backend defaults it — so the dialogs never have to send an empty list.
export type IpConfig =
  | { mode: "dhcp" }
  | { mode: "static"; cidr: string; gateway: string; dns?: string[] }
  | { mode: "disabled" };

/// Fields every link shares, in the order the dialogs collect them.
interface LinkCommon {
  ip?: IpConfig;
  comment?: string;
  mtu?: number;
}

export interface NicInput extends LinkCommon {
  name: string;
  autoneg?: boolean;
  speed?: number;
  duplex?: Duplex;
}

export interface DesiredState {
  nics: NicInput[];
  bonds: BondInput[];
  vlans: VlanInput[];
  bridges: BridgeInput[];
  management: { link: string };
}

export interface BridgeInput extends LinkCommon {
  name: string;
  ports: string[];
  stp: boolean;
  forward_delay?: number;
  vlan_filtering: boolean;
  mac_address?: string;
}

export interface BondInput extends LinkCommon {
  name: string;
  mode: BondMode;
  ports: string[];
  miimon?: number;
  lacp_rate?: "slow" | "fast";
  xmit_hash_policy?: "layer2" | "layer2+3" | "layer3+4";
  primary?: string;
}

export interface VlanInput extends LinkCommon {
  name: string;
  parent: string;
  vlan_id: number;
}

export interface NicPatch extends LinkCommon {
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

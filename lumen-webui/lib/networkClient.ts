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

/// Every function below takes an optional target `node`. Absent, the request
/// acts on the node serving the console; named, the control plane forwards it
/// to that member over the peer channel and relays the member's own answer —
/// staged there, validated there, applied inside that member's own confirm
/// window. Mutations carry it in the body (the `node` field every mutating
/// route has accepted since day one); the two reads a remote edit leans on
/// take it as `?node=`.
const nodeQuery = (node?: string): string =>
  node ? `?node=${encodeURIComponent(node)}` : "";

const withNode = <T extends object>(body: T, node?: string): T | (T & { node: string }) =>
  node ? { ...body, node } : body;

export const fetchNicPins = (node?: string): Promise<PinReport> =>
  apiFetch<PinReport>(`/network/nics/pins${nodeQuery(node)}`);

/// Give an orphaned name to a card that is actually in the machine. The
/// backend refuses a live slot or an absent adapter.
export const adoptNic = (
  slot: number,
  mac: string,
  node?: string,
): Promise<{ adopted: string; device: string; active: boolean; note?: string }> =>
  apiFetch("/network/nics/adopt", {
    method: "POST",
    body: JSON.stringify(withNode({ slot, mac }, node)),
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

const del = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, {
    method: "DELETE",
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });

export const fetchInterfaces = (): Promise<InterfacesResponse> =>
  apiFetch<InterfacesResponse>("/network/interfaces");

export const fetchPending = (node?: string): Promise<PendingResponse> =>
  apiFetch<PendingResponse>(`/network/pending${nodeQuery(node)}`);

export const fetchConfig = (): Promise<DesiredState> => apiFetch<DesiredState>("/network/config");

export const createBridge = (bridge: BridgeInput, node?: string): Promise<PendingResponse> =>
  post<PendingResponse>("/network/bridges", withNode(bridge, node));

export const createBond = (bond: BondInput, node?: string): Promise<PendingResponse> =>
  post<PendingResponse>("/network/bonds", withNode(bond, node));

export const createVlan = (vlan: VlanInput, node?: string): Promise<PendingResponse> =>
  post<PendingResponse>("/network/vlans", withNode(vlan, node));

export const updateBridge = (
  name: string,
  body: Partial<BridgeInput>,
  node?: string,
): Promise<PendingResponse> => patch<PendingResponse>(`/network/bridges/${name}`, withNode(body, node));

export const updateBond = (
  name: string,
  body: Partial<BondInput>,
  node?: string,
): Promise<PendingResponse> => patch<PendingResponse>(`/network/bonds/${name}`, withNode(body, node));

export const updateVlan = (
  name: string,
  body: Partial<VlanInput>,
  node?: string,
): Promise<PendingResponse> => patch<PendingResponse>(`/network/vlans/${name}`, withNode(body, node));

export const updateNic = (
  name: string,
  body: NicPatch,
  node?: string,
): Promise<PendingResponse> => patch<PendingResponse>(`/network/nics/${name}`, withNode(body, node));

export const deleteLink = (
  kind: LinkKind,
  name: string,
  node?: string,
): Promise<PendingResponse> =>
  del<PendingResponse>(`/network/${kind}s/${name}`, node ? { node } : undefined);

export const discardPending = (node?: string): Promise<PendingResponse> =>
  del<PendingResponse>("/network/pending", node ? { node } : undefined);

export const applyPending = (
  acknowledgeDisconnect: boolean,
  node?: string,
): Promise<ApplyResponse> =>
  post<ApplyResponse>(
    "/network/apply",
    withNode({ i_understand_this_may_disconnect_me: acknowledgeDisconnect }, node),
  );

export const confirmApply = (node?: string): Promise<PendingResponse> =>
  post<PendingResponse>("/network/confirm", node ? { node } : {});

export const rollbackApply = (node?: string): Promise<PendingResponse> =>
  post<PendingResponse>("/network/rollback", node ? { node } : {});

export const extendApply = (seconds: number, node?: string): Promise<CheckpointView> =>
  post<CheckpointView>("/network/apply/extend", withNode({ seconds }, node));

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

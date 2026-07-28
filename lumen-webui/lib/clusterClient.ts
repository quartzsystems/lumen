import { apiFetch } from "@/lib/authClient";

// Typed view of /api/environment, modelled on lib/nodeClient.ts: exported
// interfaces plus thin functions, no logic. Field names mirror lumen-cluster's
// serde output exactly, so the wire format is the only contract between the
// two.

/// What votequorum reports, read back from the running cluster.
export interface QuorumState {
  quorate: boolean;
  votes: number;
  expected_votes: number;
  /// The two-node mechanisms, present only in that regime.
  two_node: boolean;
  wait_for_all: boolean;
}

/// One knet link on one node. Link 0 rides the Core network, link 1 rides
/// Management.
export interface RingLink {
  link: number;
  address: string;
  connected: boolean;
}

export interface FenceTest {
  /// Unix seconds.
  at: number;
  passed: boolean;
}

/// One STONITH device as Pacemaker reports it. IPMI is the appliance's only
/// fence path, so a failing or never-tested device is cluster-level news.
export interface FenceDeviceState {
  device: string;
  target: string;
  active: boolean;
  failed: boolean;
  last_test: FenceTest | null;
}

/// A node the operator has taken out of service. Read off the replicated
/// record, so it is known even when the cluster itself cannot be asked.
export interface Maintenance {
  /// Unix seconds.
  since: number;
  by: string;
}

export interface ClusterNodeView {
  node: string;
  online: boolean;
  standby: boolean;
  /// Lost and not yet fenced — the state HA waits on.
  unclean: boolean;
  rings: RingLink[];
  fence?: FenceDeviceState;
  address?: string;
  controlplane_version?: string;
  maintenance?: Maintenance;
  /// This is the node whose console answered.
  local: boolean;
}

export interface FenceSummary {
  devices: number;
  healthy: number;
  failed: number;
  /// Devices never live-tested; nonzero pins the persistent warning.
  untested: number;
}

export type Regime = "two_node" | "quorum";
export type ClusterHealth = "ok" | "degraded" | "critical" | "unknown";

/// What Pacemaker reports about the cluster address resource.
///
/// `role` and `reason` are Pacemaker's own words, passed through rather than
/// mapped — the operator will search for the string Pacemaker used. `reason`
/// is the actionable half: the role says "Stopped" whatever went wrong, and
/// the reason says whether the agent is missing, timed out, or was never
/// started.
export interface VipState {
  resource: string;
  active: boolean;
  node?: string;
  failed: boolean;
  blocked: boolean;
  role?: string;
  reason?: string;
}

/// The cluster address: what the definition asked for, and what Pacemaker has
/// actually done about it.
///
/// `state` absent means the definition names a VIP and Pacemaker has no such
/// resource — a fault, not an absence.
export interface VipView {
  address: string;
  state?: VipState;
}

export interface ClusterView {
  name: string;
  regime: Regime;
  health: ClusterHealth;
  quorum: QuorumState;
  preferred_node?: string;
  nodes: ClusterNodeView[];
  fence: FenceSummary;
  /// Absent when the cluster's definition asks for no floating address.
  vip?: VipView;
  /// Why the cluster could not be asked, when it could not — its nodes are
  /// then listed from the membership record with nothing claimed about them.
  error?: string;
}

export interface EnvironmentView {
  id: string;
  version: number;
  nodes: number;
}

export interface UnassignedNodeView {
  node: string;
  address?: string;
  controlplane_version?: string;
  local: boolean;
}

/// The whole environment in one answer: grouped by cluster, then by node.
/// `environment` is absent on a node that never joined one — that node still
/// appears, as the single entry in `unassigned`.
export interface EnvironmentResponse {
  environment?: EnvironmentView;
  clusters: ClusterView[];
  unassigned: UnassignedNodeView[];
}

// --- typed networks ----------------------------------------------------------

/// One node's seat on a Core or Management network: the NIC or bond carrying
/// it, and the address it answers on there.
export interface AddressedMember {
  node: string;
  interface: string;
  address: string;
}

/// The Core network: storage replication and corosync ring 0. Subnets are
/// strings in CIDR form — the one spelling operators, the API, and
/// corosync.conf all agree on.
export interface CoreNetwork {
  subnet: string;
  mtu: number;
  members: AddressedMember[];
}

/// The Management network: the console, corosync ring 1, and the optional
/// cluster VIP.
export interface ManagementNetwork {
  subnet: string;
  vip: string | null;
  members: AddressedMember[];
}

/// One node's uplink into an External network.
export interface Uplink {
  node: string;
  interface: string;
}

/// An External network: VM traffic over an identically named bridge on every
/// member, no host addressing. The VLAN mode is flattened onto the object,
/// exactly as lumen-cluster serializes it.
export type ExternalNetwork = {
  name: string;
  bridge: string;
  uplinks: Uplink[];
} & ({ mode: "trunk"; allowed: number[] } | { mode: "access"; vlan: number });

/// Everything a cluster's members share, as one document.
export interface ClusterNetworks {
  core: CoreNetwork;
  management: ManagementNetwork;
  external: ExternalNetwork[];
}

export const fetchClusterNetworks = (name: string): Promise<ClusterNetworks> =>
  apiFetch<ClusterNetworks>(`/environment/clusters/${encodeURIComponent(name)}/networks`);

/// An External network to define, with one uplink per member.
///
/// Every member or none: an External network that exists on some members is
/// one a machine can be restarted onto a node where its network does not
/// resolve, which is the failure HA exists to prevent. The dialog collects an
/// uplink for every member for that reason, and the backend refuses anything
/// short.
export type ExternalNetworkCreate = {
  name: string;
  bridge: string;
  uplinks: Uplink[];
} & ({ mode: "trunk"; allowed: number[] } | { mode: "access"; vlan: number });

/// Define an External network and realize its bridge on every member.
///
/// Not a definition-only write: the record and the boxes are made to agree
/// before this answers, because a definition every member has not realized is
/// exactly the inconsistency the consistency rule forbids. A member that
/// cannot build its bridge fails the whole call and the definition is not
/// recorded.
export const createExternalNetwork = (
  cluster: string,
  network: ExternalNetworkCreate,
): Promise<ExternalNetwork> =>
  post(`/environment/clusters/${encodeURIComponent(cluster)}/networks/external`, network);

// --- workflows ---------------------------------------------------------------

export interface MintedToken {
  token: string;
  /// Unix seconds.
  expires_at: number;
  bootstrapped: boolean;
}

/// One link as a preflighted node reports it — the wizard's NIC pickers read
/// these. A subset of lumen-net's ObservedLink; only what the pickers show.
export interface PreflightLink {
  name: string;
  kind: string;
  carrier: boolean;
  addresses: string[];
  mtu?: number | null;
  /// The bridge or bond that has already claimed this link, when one has. A
  /// link answers to one controller, so a claimed one cannot be bonded again.
  controller?: string | null;
}

/// The links that can hold a ring seat. `other` is everything the appliance
/// did not create and has no profile for — loopback, guest taps, tunnels —
/// and a seat placed there fails at prepare time, so it is not offered.
/// A bond belongs here: bonding two NICs is how a ring survives a cable.
export const seatableLinks = (links: PreflightLink[]): PreflightLink[] =>
  links.filter((link) => link.kind !== "other");

/// How a link reads in a ring picker: its name, what it is when that is not
/// a plain NIC, and the fact that will bite — no carrier, or the address it
/// already answers on.
export const linkLabel = (link: PreflightLink, show: "carrier" | "address"): string => {
  const parts = [link.name];
  if (link.kind !== "ethernet") parts.push(link.kind);
  if (show === "carrier" && !link.carrier) parts.push("no carrier");
  if (show === "address" && link.addresses.length > 0) parts.push(link.addresses[0]);
  return parts.join(" — ");
};

export interface PreflightReport {
  node: string;
  controlplane_version: string;
  hostname: string;
  time_synchronized: boolean;
  time_offset_ms?: number;
  already_clustered: boolean;
  links: PreflightLink[];
}

export interface PreflightView {
  node: string;
  ok: boolean;
  problems: string[];
  report?: PreflightReport;
}

export interface MemberCreate {
  node: string;
  core_interface: string;
  core_address: string;
  management_interface: string;
  management_address: string;
  bmc_address: string;
  bmc_username: string;
  /// Written into the CIB by the fencing stage; never stored anywhere else,
  /// which is why editing a cluster later never shows it back.
  bmc_password: string;
}

export interface ClusterCreateRequest {
  name: string;
  preferred_node?: string | null;
  core: { subnet: string; mtu: number };
  management: { subnet: string; vip?: string | null };
  members: MemberCreate[];
}

export type StepState = "pending" | "running" | "done" | "failed" | "unwound";
export type WorkflowPhase = "running" | "complete" | "failed";

export interface StepProgress {
  step: string;
  node?: string;
  state: StepState;
  detail?: string;
}

export interface CreateProgress {
  cluster: string;
  phase: WorkflowPhase;
  error?: string;
  steps: StepProgress[];
}

export const fetchEnvironment = (): Promise<EnvironmentResponse> =>
  apiFetch<EnvironmentResponse>("/environment");

export const fetchCluster = (name: string): Promise<ClusterView> =>
  apiFetch<ClusterView>(`/environment/clusters/${encodeURIComponent(name)}`);

const post = <T>(path: string, body?: unknown): Promise<T> =>
  apiFetch<T>(path, { method: "POST", body: body === undefined ? undefined : JSON.stringify(body) });

export const mintToken = (): Promise<MintedToken> => post<MintedToken>("/environment/tokens");

export const joinEnvironment = (token: string): Promise<{ joined: boolean; note: string }> =>
  post("/environment/join", { token });

export const preflightNodes = (nodes: string[]): Promise<PreflightView[]> =>
  post("/environment/preflight", { nodes });

/// Bond two or more of a node's NICs, before it is a cluster member — the
/// wizard's shortcut to a Core seat that survives a cable. It lands in the
/// target node's own networking domain, so the bond that results is an
/// ordinary link, edited and deleted from its Networking page like any other.
export const bondNodeNics = (
  node: string,
  bond: BondNodeRequest,
): Promise<{ bonded: boolean }> =>
  post(`/environment/nodes/${encodeURIComponent(node)}/bond`, bond);

export interface BondNodeRequest {
  name: string;
  mode: "active-backup" | "802.3ad" | "balance-xor";
  ports: string[];
  miimon?: number;
  mtu?: number;
}

export const createCluster = (request: ClusterCreateRequest): Promise<CreateProgress> =>
  post("/environment/clusters", request);

export const fetchCreateProgress = (): Promise<CreateProgress> =>
  apiFetch<CreateProgress>("/environment/clusters/pending");

/// 202 with the same progress feed a create publishes; poll
/// fetchCreateProgress until the phase leaves "running".
export const addClusterNode = (
  cluster: string,
  member: MemberCreate,
): Promise<CreateProgress> =>
  post(`/environment/clusters/${encodeURIComponent(cluster)}/nodes`, member);

export const destroyCluster = (name: string, acknowledge: boolean): Promise<void> =>
  apiFetch<void>(`/environment/clusters/${encodeURIComponent(name)}`, {
    method: "DELETE",
    body: JSON.stringify({ i_understand_this_may_lose_data: acknowledge }),
  });

export const removeNode = (name: string): Promise<void> =>
  apiFetch<void>(`/environment/nodes/${encodeURIComponent(name)}`, { method: "DELETE" });

/// What a guarded live fence test answered. `passed: false` is a successful
/// request that learned something bad about the fence path.
export interface FenceTestView {
  cluster: string;
  node: string;
  passed: boolean;
  /// Unix seconds.
  at: number;
  error?: string;
}

export const testFence = (cluster: string, node: string): Promise<FenceTestView> =>
  post(
    `/environment/clusters/${encodeURIComponent(cluster)}/fence/${encodeURIComponent(node)}/test`,
    { i_understand_this_power_cycles_the_node: true },
  );

// --- maintenance -------------------------------------------------------------

/// One machine still running on the node after a drain, and why it did not
/// move. An empty list is what "safe to restart" means.
export interface Stranded {
  vmid: number;
  name: string;
  reason: string;
}

/// A drain, in the same shape a cluster create publishes — one step per
/// machine, plus the step for taking the node out of service.
export interface MaintenanceProgress {
  node: string;
  cluster: string;
  phase: WorkflowPhase;
  error?: string;
  steps: StepProgress[];
  stranded: Stranded[];
}

/// What the node's own maintenance state is, with no drain attached.
export interface MaintenanceView {
  node: string;
  cluster: string;
  maintenance?: Maintenance;
  /// Whether the cluster stays quorate without this node's vote — what
  /// decides whether the reboot after the drain is safe.
  quorum_safe: boolean;
}

const maintenancePath = (cluster: string, node: string): string =>
  `/environment/clusters/${encodeURIComponent(cluster)}/nodes/${encodeURIComponent(node)}/maintenance`;

/// 202 with the first progress when the machines are moving, 200 with the
/// finished one when they were left alone. Poll `fetchDrain` either way.
export const enterMaintenance = (
  cluster: string,
  node: string,
  evacuate: boolean,
): Promise<MaintenanceProgress> => post(maintenancePath(cluster, node), { evacuate });

export const exitMaintenance = (cluster: string, node: string): Promise<MaintenanceView> =>
  apiFetch<MaintenanceView>(maintenancePath(cluster, node), { method: "DELETE" });

/// The drain of the node this console is talking to, or null if it has not
/// been drained since its control plane started.
export const fetchDrain = (): Promise<MaintenanceProgress | null> =>
  apiFetch<MaintenanceProgress | null>("/environment/maintenance");

export const confirmNodeDead = (cluster: string, node: string): Promise<{ confirmed: boolean }> =>
  post(
    `/environment/clusters/${encodeURIComponent(cluster)}/nodes/${encodeURIComponent(node)}/confirm-dead`,
    { i_have_verified_the_node_is_powered_off: true },
  );

// --- display helpers ---------------------------------------------------------

export const HEALTH_TONE: Record<ClusterHealth, "ok" | "warn" | "crit" | "muted"> = {
  ok: "ok",
  degraded: "warn",
  critical: "crit",
  unknown: "muted",
};

export const HEALTH_LABEL: Record<ClusterHealth, string> = {
  ok: "Healthy",
  degraded: "Degraded",
  critical: "Critical",
  unknown: "Unreachable",
};

export const REGIME_LABEL: Record<Regime, string> = {
  two_node: "2-node, witness-less",
  quorum: "Majority quorum",
};

/// The state pill for one member row, most alarming condition first.
///
/// Maintenance outranks standby and offline but not unfenced: a node the
/// operator took out of service is expected to be both of those, and saying
/// "Offline" about a node somebody is deliberately working on hides the one
/// fact that explains the rest of the row. Unfenced still wins, because a node
/// waiting on a fence is a data-integrity problem whoever put it there.
export const nodeTone = (node: ClusterNodeView): { tone: string; label: string } => {
  if (node.unclean) return { tone: "crit", label: "Unfenced" };
  if (node.maintenance) return { tone: "warn", label: "Maintenance" };
  if (!node.online) return { tone: "muted", label: "Offline" };
  if (node.standby) return { tone: "warn", label: "Standby" };
  return { tone: "ok", label: "Online" };
};

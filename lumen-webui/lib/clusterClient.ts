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

export interface ClusterView {
  name: string;
  regime: Regime;
  health: ClusterHealth;
  quorum: QuorumState;
  preferred_node?: string;
  nodes: ClusterNodeView[];
  fence: FenceSummary;
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
}

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

export const createCluster = (request: ClusterCreateRequest): Promise<CreateProgress> =>
  post("/environment/clusters", request);

export const fetchCreateProgress = (): Promise<CreateProgress> =>
  apiFetch<CreateProgress>("/environment/clusters/pending");

export const destroyCluster = (name: string, acknowledge: boolean): Promise<void> =>
  apiFetch<void>(`/environment/clusters/${encodeURIComponent(name)}`, {
    method: "DELETE",
    body: JSON.stringify({ i_understand_this_may_lose_data: acknowledge }),
  });

export const removeNode = (name: string): Promise<void> =>
  apiFetch<void>(`/environment/nodes/${encodeURIComponent(name)}`, { method: "DELETE" });

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
export const nodeTone = (node: ClusterNodeView): { tone: string; label: string } => {
  if (node.unclean) return { tone: "crit", label: "Unfenced" };
  if (!node.online) return { tone: "muted", label: "Offline" };
  if (node.standby) return { tone: "warn", label: "Standby" };
  return { tone: "ok", label: "Online" };
};

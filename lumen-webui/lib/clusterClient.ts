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

export const fetchEnvironment = (): Promise<EnvironmentResponse> =>
  apiFetch<EnvironmentResponse>("/environment");

export const fetchCluster = (name: string): Promise<ClusterView> =>
  apiFetch<ClusterView>(`/environment/clusters/${encodeURIComponent(name)}`);

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

import { apiFetch } from "@/lib/authClient";

// Typed view of /api/nodes, modelled on lib/vmClient.ts: exported interfaces
// plus thin functions, no logic. Field names mirror lumen-virt's serde output
// exactly, so the wire format is the only contract between the two.

/// One node's capacity, and what the machines on it are holding.
export interface NodeView {
  node: string;
  /// Logical processors the node has online.
  cpus: number;
  memory_mib: number;
  /// Memory kept back for the node itself, which no machine may be given.
  reserved_memory_mib: number;
  hypervisor_version: string | null;
  /// Machines defined here, and how many of them are running.
  machines: number;
  running: number;
  /// What the running machines hold between them, as the hypervisor reports
  /// it. `used_vcpus` may exceed `cpus` — processors are overcommittable, and
  /// a node that has been overcommitted must read as overcommitted rather than
  /// as full.
  used_vcpus: number;
  used_memory_mib: number;
}

/// Grouped as a list from day one — one node today, the same shape when
/// clustering lands.
export interface NodesResponse {
  nodes: NodeView[];
}

export const fetchNodes = (): Promise<NodesResponse> => apiFetch<NodesResponse>("/nodes");

// --- display helpers ---------------------------------------------------------

/// Memory a machine may actually be given: everything but the node's own
/// reserve. This is the denominator the dashboard's memory meter uses, because
/// it is the number that runs out first.
export const assignableMemoryMib = (node: NodeView): number =>
  Math.max(0, node.memory_mib - node.reserved_memory_mib);

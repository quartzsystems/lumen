import type { AddressedMember, ClusterView, RingLink, VipView } from "@/lib/clusterClient";
import type { InventoryResponse } from "@/lib/inventoryClient";

// What corosync sees of a cluster's addressed networks, in one place because
// two pages ask it — the dashboard's networks panel and Networking → Networks
// — and a rule about when a link may be called "down" is exactly the kind of
// thing that goes wrong when it is written twice.

export type StatusTone = "ok" | "warn" | "crit" | "muted";

export interface RingStatus {
  tone: StatusTone;
  status: string;
}

/// Every member's rings, gathered from wherever they can be had.
///
/// `corosync-cfgtool -s` answers for the node it runs on and no other, so a
/// cluster view assembled from one member carries one member's link health.
/// The environment-wide inventory asks every member at once and each answers
/// for itself, which is what fills the gaps — so the inventory wins where it
/// has something to say, and the cluster view is the fallback for a member
/// that could not be asked.
export function ringsByNode(
  cluster: ClusterView,
  inventory: InventoryResponse | null,
): Map<string, RingLink[]> {
  const rings = new Map<string, RingLink[]>();
  for (const node of cluster.nodes) {
    if (node.rings.length > 0) rings.set(node.node, node.rings);
  }
  for (const member of inventory?.members ?? []) {
    const own = member.inventory?.rings ?? [];
    if (own.length > 0) rings.set(member.node, own);
  }
  return rings;
}

/// Every member's seat on the ring a network carries.
///
/// A cluster that could not be asked, or a member nothing has anything to say
/// about, is unknown — this may not report a link as up or down on the
/// strength of a definition alone.
///
/// The ratio counts only members that were actually observed. A member whose
/// rings nobody could report is unknown rather than down; counting it in the
/// denominator would read a two-node cluster with one node rebooting as "1 /
/// 2 up", which is a link fault, not an absent node.
export function ringState(
  cluster: ClusterView,
  ring: number,
  members: AddressedMember[],
  /// Every member's rings, from `ringsByNode`. Omitted falls back to what the
  /// cluster view alone carries, which is the local node's.
  rings?: Map<string, RingLink[]>,
): RingStatus {
  if (cluster.error || members.length === 0) return { tone: "muted", status: "Unknown" };
  const known = rings ?? ringsByNode(cluster, null);

  let observed = 0;
  let connected = 0;
  for (const member of members) {
    const link = known.get(member.node)?.find((candidate) => candidate.link === ring);
    if (!link) continue;
    observed += 1;
    if (link.connected) connected += 1;
  }

  if (observed === 0) return { tone: "muted", status: "Unknown" };
  if (connected === 0) return { tone: "crit", status: "Down" };
  if (connected < observed) return { tone: "warn", status: `${connected} / ${observed} up` };

  const silent = members.length - observed;
  return silent === 0
    ? { tone: "ok", status: "Connected" }
    : { tone: "ok", status: `Connected · ${silent} unknown` };
}

/// The cluster VIP, as Pacemaker has it — not as the definition claims it.
///
/// This is the distinction that matters: a definition names an address, and
/// only Pacemaker knows whether anything answers on it. Reporting the
/// definition alone is how a cluster shows a healthy VIP for an address that
/// has never once come up.
///
/// The reason travels into the status text because "Stopped" is the same word
/// for every cause, and the cause is what an operator has to act on.
export function vipState(cluster: ClusterView, vip: VipView): RingStatus {
  if (cluster.error) return { tone: "muted", status: "Unknown" };
  if (!vip.state) {
    // The definition asks for an address and Pacemaker has no resource for
    // it at all — nothing is going to bring it up on its own.
    return { tone: "crit", status: "Not configured" };
  }
  const { active, failed, blocked, role, reason } = vip.state;
  if (active && !failed && !blocked) return { tone: "ok", status: "Started" };

  const detail = reason ?? role ?? "Stopped";
  if (blocked) return { tone: "crit", status: `Blocked · ${detail}` };
  if (failed) return { tone: "crit", status: `Failed · ${detail}` };
  return { tone: "crit", status: detail };
}

/// An External network carries no ring and no host addressing, so corosync has
/// nothing to say about it. What can be checked is whether the definition is
/// complete: the consistency rule HA depends on is that the network exists on
/// every member or on none.
export function externalState(
  cluster: ClusterView,
  uplinkNodes: string[],
): RingStatus {
  const members = cluster.nodes.length;
  if (members === 0) return { tone: "muted", status: "Unknown" };
  if (uplinkNodes.length >= members) return { tone: "ok", status: "On every member" };
  if (uplinkNodes.length === 0) return { tone: "crit", status: "On no member" };
  return { tone: "warn", status: `${uplinkNodes.length} / ${members} members` };
}

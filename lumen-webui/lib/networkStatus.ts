import type { AddressedMember, ClusterView, VipView } from "@/lib/clusterClient";

// What corosync sees of a cluster's addressed networks, in one place because
// two pages ask it — the dashboard's networks panel and Networking → Networks
// — and a rule about when a link may be called "down" is exactly the kind of
// thing that goes wrong when it is written twice.

export type StatusTone = "ok" | "warn" | "crit" | "muted";

export interface RingStatus {
  tone: StatusTone;
  status: string;
}

/// Every member's seat on the ring a network carries.
///
/// A cluster that could not be asked, or a member the cluster has nothing to
/// say about, is unknown — this may not report a link as up or down on the
/// strength of a definition alone.
///
/// The ratio counts only members that were actually observed. `corosync-cfgtool`
/// answers for the local node alone, so until federation carries peers' rings
/// the members it said nothing about are unknown rather than down; counting
/// them in the denominator reads a healthy two-node cluster as "1 / 2 up".
export function ringState(
  cluster: ClusterView,
  ring: number,
  members: AddressedMember[],
): RingStatus {
  if (cluster.error || members.length === 0) return { tone: "muted", status: "Unknown" };

  let observed = 0;
  let connected = 0;
  for (const member of members) {
    const node = cluster.nodes.find((candidate) => candidate.node === member.node);
    const link = node?.rings.find((candidate) => candidate.link === ring);
    if (!node || !link) continue;
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

/// The cluster address, as Pacemaker has it — not as the definition claims it.
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

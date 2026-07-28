import { apiFetch } from "@/lib/authClient";
import type { RingLink } from "@/lib/clusterClient";
import type { LinkView } from "@/lib/networkClient";
import type { NodeView } from "@/lib/nodeClient";
import type { BlockDevice, Compression, PoolView, VdevKind } from "@/lib/storageClient";

// Typed view of /api/environment/inventory: what every member of the
// environment has, side by side. The node-local reads — /api/nodes,
// /api/network/interfaces, /api/storage/pools — still answer for this
// appliance alone, because the edits the console offers on a link or a pool
// land on the node that owns it. This is the read that spans them.

/// One node's answer: what it is, what it is plugged into, and what it can
/// store.
export interface NodeInventory {
  node: string;
  /// Absent when the hypervisor could not be asked. A node whose libvirt is
  /// down still has links and pools worth showing.
  capacity?: NodeView;
  interfaces: LinkView[];
  pools: PoolView[];
  /// Every disk the node has, claimed and unclaimed. The drive picker needs
  /// them across members at one moment, which is why they ride here rather
  /// than costing a request per node.
  devices: BlockDevice[];
  /// This node's own corosync links, one per ring. Here for the same reason
  /// the disks are: `corosync-cfgtool` speaks only for the node it runs on,
  /// so link health across a cluster can only be had by asking every member
  /// at once.
  rings: RingLink[];
}

/// One member, or the reason it has no inventory.
///
/// `reachable: false` is a member that is in the environment and could not be
/// asked, which is a different fact from a member with nothing on it — one is
/// a warning on the row, the other is an empty cell.
export interface MemberInventory {
  node: string;
  /// The node serving this request: the only one whose links and pools the
  /// console may offer to edit.
  local: boolean;
  reachable: boolean;
  error?: string;
  inventory?: NodeInventory;
}

export interface InventoryResponse {
  members: MemberInventory[];
}

export const fetchInventory = (): Promise<InventoryResponse> =>
  apiFetch<InventoryResponse>("/environment/inventory");

/// One member's share of a cluster-wide pool build.
export interface PoolSeat {
  node: string;
  disks: string[];
}

/// Build one identically named pool across several members at once.
///
/// There is no cluster-wide pool at the end of this. Each member gets its
/// own, on its own disks, listed and destroyed on its own Storage page. What
/// it buys is that the names match — a replicated volume names one pool and
/// means it on every member holding a replica, and doing this by hand across
/// consoles is where the names drift apart.
export interface ClusterPoolCreate {
  name: string;
  vdev: VdevKind;
  compression: Compression;
  ashift?: number;
  seats: PoolSeat[];
  i_understand_this_erases_the_disks: boolean;
}

export interface ClusterPoolOutcome {
  name: string;
  /// The members that got one, in the order they were built.
  built: string[];
}

export const createClusterPool = (
  request: ClusterPoolCreate,
): Promise<ClusterPoolOutcome> =>
  apiFetch<ClusterPoolOutcome>("/environment/storage/pools", {
    method: "POST",
    body: JSON.stringify(request),
  });

// --- display helpers ---------------------------------------------------------

/// Every member's links, flattened, each carrying the node it belongs to.
///
/// The Interfaces table is one table across the environment rather than one
/// per node, so the node stops being a heading and becomes a column — and a
/// row has to carry enough to know whether it may be edited from here.
export interface OwnedLink {
  node: string;
  local: boolean;
  link: LinkView;
}

export const linksByMember = (inventory: InventoryResponse | null): OwnedLink[] =>
  (inventory?.members ?? []).flatMap((member) =>
    (member.inventory?.interfaces ?? []).map((link) => ({
      node: member.node,
      local: member.local,
      link,
    })),
  );

/// One member's disks, with the node they belong to — what the cross-node
/// drive picker lists.
export interface OwnedDevice {
  node: string;
  device: BlockDevice;
}

export const devicesByMember = (inventory: InventoryResponse | null): OwnedDevice[] =>
  (inventory?.members ?? []).flatMap((member) =>
    (member.inventory?.devices ?? []).map((device) => ({ node: member.node, device })),
  );

/// One member's pools, with the node they belong to.
///
/// The same shape as the links: the Pools table spans the environment, so the
/// node stops being a heading and becomes a column. `local` rides along
/// because destroying a pool still lands on the node that owns it.
export interface OwnedPool {
  node: string;
  local: boolean;
  pool: PoolView;
}

export const poolsByMember = (inventory: InventoryResponse | null): OwnedPool[] =>
  (inventory?.members ?? []).flatMap((member) =>
    (member.inventory?.pools ?? []).map((pool) => ({
      node: member.node,
      local: member.local,
      pool,
    })),
  );

/// Clear one member's disk: its partition table and every filesystem and pool
/// signature on it.
///
/// Destructive and not undoable, which is why the acknowledgement is spelled
/// out in the name of the field rather than being a bare boolean. The owning
/// node applies its own guards regardless — a disk holding a pool, a mount, or
/// swap is refused there, whatever this console believed when it asked.
export const wipeNodeDisk = (node: string, disk: string): Promise<BlockDevice> =>
  apiFetch<BlockDevice>(
    `/environment/nodes/${encodeURIComponent(node)}/disks/${encodeURIComponent(disk)}/wipe`,
    {
      method: "POST",
      body: JSON.stringify({ i_understand_this_may_lose_data: true }),
    },
  );

/// What the members' pools add up to.
///
/// Raw, and it matters that the caller says so. A replicated volume consumes
/// its full size on every member holding a replica, so this is what the
/// hardware is — not what a machine may be given. The usable figure depends
/// on per-volume replica count and is not a property of the disks.
export interface PooledStorage {
  /// Members that answered, and members there are.
  counted: number;
  of: number;
  pools: number;
  size: number;
  allocated: number;
  free: number;
}

export const pooledStorage = (inventory: InventoryResponse | null): PooledStorage => {
  const members = inventory?.members ?? [];
  const total: PooledStorage = {
    counted: 0,
    of: members.length,
    pools: 0,
    size: 0,
    allocated: 0,
    free: 0,
  };
  for (const member of members) {
    if (!member.inventory) continue;
    total.counted += 1;
    for (const pool of member.inventory.pools) {
      total.pools += 1;
      total.size += pool.size;
      total.allocated += pool.allocated;
      total.free += pool.free;
    }
  }
  return total;
};

/// Members that are in the environment and could not be asked. The page shows
/// these by name: a table that is quietly missing a node's rows is worse than
/// one that says which node is missing.
export const unreachable = (inventory: InventoryResponse | null): MemberInventory[] =>
  (inventory?.members ?? []).filter((member) => !member.reachable);

/// What a cluster's members add up to.
///
/// Storage is deliberately *raw* capacity. A replicated volume consumes its
/// full size on every member holding a replica, so the sum of the pools is
/// what the hardware is, not what a machine may be given — and a figure that
/// implied otherwise would be wrong by exactly the replica count.
export interface PooledCapacity {
  /// Members that answered, and members the cluster has.
  counted: number;
  of: number;
  cores: number;
  usedCores: number;
  memoryMib: number;
  usedMemoryMib: number;
  rawStorage: number;
  allocatedStorage: number;
}

export const pooledCapacity = (
  inventory: InventoryResponse | null,
  members: string[],
): PooledCapacity => {
  const wanted = new Set(members);
  const rows = (inventory?.members ?? []).filter((member) => wanted.has(member.node));

  const total: PooledCapacity = {
    counted: 0,
    of: members.length,
    cores: 0,
    usedCores: 0,
    memoryMib: 0,
    usedMemoryMib: 0,
    rawStorage: 0,
    allocatedStorage: 0,
  };

  for (const member of rows) {
    if (!member.inventory) continue;
    total.counted += 1;
    const capacity = member.inventory.capacity;
    if (capacity) {
      total.cores += capacity.cpus;
      total.usedCores += capacity.used_vcpus;
      total.memoryMib += capacity.memory_mib;
      total.usedMemoryMib += capacity.used_memory_mib;
    }
    for (const pool of member.inventory.pools) {
      total.rawStorage += pool.size;
      total.allocatedStorage += pool.allocated;
    }
  }

  return total;
};

"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { AlertTriangle } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import {
  Bar,
  DashPanel,
  MoreLink,
  PanelEmpty,
  StatTile,
  Status,
  staticRow,
  type Tone,
} from "@/components/dashboard/DashboardBits";
import { deriveAlarms, SEVERITY_TONE, worstSeverity, type Alarm } from "@/lib/alarms";
import { ApiError } from "@/lib/authClient";
import {
  fetchClusterNetworks,
  fetchEnvironment,
  HEALTH_LABEL,
  HEALTH_TONE,
  REGIME_LABEL,
  type AddressedMember,
  type ClusterNetworks,
  type ClusterView,
} from "@/lib/clusterClient";
import {
  fetchInterfaces,
  fetchPending,
  type LinkView,
  type PendingResponse,
} from "@/lib/networkClient";
import { assignableMemoryMib, fetchNodes, type NodeView } from "@/lib/nodeClient";
import { fetchPools, type PoolView } from "@/lib/storageClient";
import { useVms } from "@/lib/VmContext";
import { fetchRecentTasks, formatBytes, formatMib, type TaskView } from "@/lib/vmClient";
import {
  fetchInventory,
  pooledCapacity,
  type InventoryResponse,
  type PooledCapacity,
} from "@/lib/inventoryClient";
import { fetchPooledStorage, type PooledStorageView } from "@/lib/poolClient";
import { ringsByNode, ringState, vipState } from "@/lib/networkStatus";

/// Matches lib/VmContext.tsx: often enough that the page feels live, slow
/// enough to cost nothing.
const POLL_MS = 5000;

/// How much of each list a panel shows before sending the operator to the page
/// that owns it in full.
const NETWORK_ROWS = 8;
const LOG_ROWS = 12;

/// One row of the networks panel: a cluster-wide network, not a node's link.
/// The panel ranks by the order the three types are defined in — Core, then
/// Management, then External — because there are no traffic counters behind
/// this page to sort on, and inventing an order that looks like throughput
/// would be a lie.
interface NetworkRow {
  key: string;
  /// Which cluster defines it. Shown only when the environment has more than
  /// one, the way Networking → Networks names its sections.
  cluster: string;
  name: string;
  kind: "Core" | "Management" | "External";
  /// The one fact that distinguishes this network from another of its type.
  qualifier: string | null;
  /// Null where the network has no host addressing at all, which is what an
  /// External network is.
  address: string | null;
  /// A second address the same network answers on — the cluster VIP, which is
  /// the only one of these there is.
  alsoAt: string | null;
  tone: Tone;
  status: string;
}

/// The console's landing page: what this appliance is running, what is wrong
/// with it, and what has been done to it lately.
///
/// Every number here is read from the subsystem that owns it rather than
/// stored — there is no dashboard service, and nothing on this page is
/// authoritative about anything. Panels whose subsystem does not exist yet say
/// so in the same words the rest of the console uses.
export default function DashboardPage() {
  const router = useRouter();
  const { vms, error: vmsError } = useVms();

  const [capacity, setCapacity] = useState<NodeView[] | null>(null);
  const [links, setLinks] = useState<LinkView[] | null>(null);
  const [pending, setPending] = useState<PendingResponse | null>(null);
  const [pools, setPools] = useState<PoolView[] | null>(null);
  const [tasks, setTasks] = useState<TaskView[] | null>(null);
  const [clusters, setClusters] = useState<ClusterView[] | null>(null);
  /// The networks document per cluster, keyed by cluster name. A cluster whose
  /// record could not be read is simply absent — its rows go missing rather
  /// than being invented.
  const [networks, setNetworks] = useState<Record<string, ClusterNetworks>>({});
  /// Whether a networks pass has finished at all. Without it an empty panel
  /// cannot tell "not asked yet" from "asked, and nobody would answer".
  const [networksAsked, setNetworksAsked] = useState(false);
  /// Which subsystems could not be read this poll. Named rather than counted:
  /// "storage could not be read" is actionable and "0 pools" is a lie.
  const [unread, setUnread] = useState<string[]>([]);
  /// What every member has, which is the only way a cluster row can add up
  /// its members' processors, memory, and disks.
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  /// The LumenFS pool this node serves, if any. Its bricks are not zpools, so
  /// the inventory's sums know nothing about it — the cluster row asks here
  /// for the replicated capacity.
  const [lumenPool, setLumenPool] = useState<PooledStorageView | null>(null);

  // Settled rather than all: a dashboard is six independent readings, and one
  // subsystem being down must not blank the five that are up.
  const load = useCallback(async () => {
    const [nodes, interfaces, staged, storage, log, environment, everyone, pool] =
      await Promise.allSettled([
        fetchNodes(),
        fetchInterfaces(),
        fetchPending(),
        fetchPools(),
        fetchRecentTasks(LOG_ROWS),
        fetchEnvironment(),
        fetchInventory(),
        fetchPooledStorage(),
      ]);

    const failed: string[] = [];
    const take = <T,>(
      result: PromiseSettledResult<T>,
      subsystem: string,
      apply: (value: T) => void,
    ) => {
      if (result.status === "fulfilled") {
        apply(result.value);
        return;
      }
      // A 401 has already redirected to /login; there is nothing to report and
      // this page is about to be unmounted anyway.
      if (result.reason instanceof ApiError && result.reason.status === 401) return;
      failed.push(subsystem);
    };

    take(nodes, "the node", (value) => setCapacity(value.nodes));
    take(interfaces, "networking", (value) =>
      setLinks(value.nodes.flatMap((node) => node.interfaces)),
    );
    take(staged, "staged network changes", setPending);
    take(storage, "storage", (value) => setPools(value.nodes.flatMap((node) => node.pools)));
    take(log, "the log", (value) => setTasks(value.tasks));
    take(environment, "the environment", (value) => setClusters(value.clusters));
    take(everyone, "the other members", setInventory);
    // `pool: null` is the ordinary unpooled node, not a failure — only a
    // request that errored joins the unread list.
    take(pool, "pooled storage", (value) => setLumenPool(value.pool));
    setUnread(failed);

    // The shared network definitions live one request per cluster behind the
    // environment, so they can only be asked for once it has answered. A
    // cluster that will not hand its record over drops out of the map and out
    // of the panel with it.
    if (environment.status === "fulfilled") {
      const documents = await Promise.all(
        environment.value.clusters.map(async (cluster) => {
          try {
            return [cluster.name, await fetchClusterNetworks(cluster.name)] as const;
          } catch {
            return [cluster.name, null] as const;
          }
        }),
      );
      setNetworks(
        Object.fromEntries(
          documents.filter((entry): entry is [string, ClusterNetworks] => entry[1] !== null),
        ),
      );
      setNetworksAsked(true);
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const running = vms.filter((vm) => vm.state === "running").length;

  const alarms = useMemo(
    () =>
      deriveAlarms({
        vms,
        pools: pools ?? undefined,
        links: links ?? undefined,
        pending,
        nodes: capacity ?? undefined,
      }),
    [vms, pools, links, pending, capacity],
  );
  const worst = worstSeverity(alarms);

  /// Every member of the environment, not just the one serving this page.
  ///
  /// Falls back to the node-local reading when the environment read failed,
  /// so a standalone appliance — and a console whose peers are all away —
  /// still shows the node the operator is actually looking at.
  const members = useMemo(() => {
    if (inventory !== null) {
      return inventory.members.map((member) => ({
        node: member.node,
        reachable: member.reachable,
        capacity: member.inventory?.capacity ?? null,
      }));
    }
    if (capacity === null) return null;
    return capacity.map((node) => ({ node: node.node, reachable: true, capacity: node }));
  }, [inventory, capacity]);

  const membersOnline = members?.filter((member) => member.reachable).length ?? 0;

  // Every cluster's networks, in the order the three types are defined, with
  // the live ring state each addressed network's members are sitting on.
  const clusterNetworks = useMemo(() => {
    const rows: NetworkRow[] = [];
    for (const cluster of clusters ?? []) {
      const shared = networks[cluster.name];
      if (!shared) continue;
      rows.push({
        key: `${cluster.name}/core`,
        cluster: cluster.name,
        name: "Core",
        kind: "Core",
        qualifier: `MTU ${shared.core.mtu}`,
        address: shared.core.subnet,
        alsoAt: null,
        ...ringState(cluster, 0, shared.core.members, ringsByNode(cluster, inventory)),
      });
      rows.push({
        key: `${cluster.name}/management`,
        cluster: cluster.name,
        name: "Management",
        kind: "Management",
        qualifier: null,
        address: shared.management.subnet,
        alsoAt: null,
        ...ringState(cluster, 1, shared.management.members, ringsByNode(cluster, inventory)),
      });
      // The floating address, with what Pacemaker has actually done about it.
      // Its own row and its own state: the ring being up says nothing about
      // whether anything answers on the VIP, and this panel used to print the
      // address beside the Management subnet as though naming it made it so.
      if (cluster.vip) {
        rows.push({
          key: `${cluster.name}/vip`,
          cluster: cluster.name,
          name: "Cluster VIP",
          kind: "Management",
          qualifier: null,
          address: cluster.vip.address,
          alsoAt: cluster.vip.state?.node ? `on ${cluster.vip.state.node}` : null,
          ...vipState(cluster, cluster.vip),
        });
      }
      for (const external of shared.external) {
        rows.push({
          key: `${cluster.name}/external/${external.name}`,
          cluster: cluster.name,
          name: external.name,
          kind: "External",
          qualifier:
            external.mode === "trunk"
              ? external.allowed.length > 0
                ? `Trunk ${external.allowed.join(", ")}`
                : "Trunk"
              : `VLAN ${external.vlan}`,
          // No host addressing by definition, and nothing watches an External
          // bridge yet — so the row claims neither.
          address: null,
          alsoAt: null,
          tone: "muted",
          status: "Defined",
        });
      }
    }
    return rows;
  }, [clusters, networks]);

  const topNetworks = clusterNetworks.slice(0, NETWORK_ROWS);
  const clustersHealthy = (clusters ?? []).filter((cluster) => cluster.health === "ok").length;

  // The log records identifiers, not names, because a machine can be renamed
  // and its history must not be rewritten when it is. The name is looked up
  // for display only, and a machine that has since been removed keeps its
  // number.
  const nameOf = useMemo(() => {
    const names = new Map(vms.map((vm) => [vm.vmid, vm.name]));
    return (vmid: number) => names.get(vmid) ?? null;
  }, [vms]);

  return (
    <Page>
      <PageHeader
        title="Dashboard"
        description="What this appliance is running, and what has been done to it."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {/* One line per subsystem that could not be read. A dashboard that
              renders a stale panel as though it were current is worse than one
              that says which reading it is missing. */}
          {(vmsError || unread.length > 0) && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="flex flex-col gap-1 text-[13px] text-[var(--qz-fg-2)]">
                {vmsError && <span>{vmsError}</span>}
                {unread.length > 0 && (
                  <span>
                    Could not read {unread.join(", ")} — those panels are showing the last reading
                    that worked.
                  </span>
                )}
              </div>
            </div>
          )}

          <div
            className="grid gap-4"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(190px, 1fr))" }}
          >
            <StatTile
              label="Virtual Machines"
              href="/virtual-machines"
              value={String(running)}
              of={String(vms.length)}
              // Machines running is a good number to be high, so it never goes
              // amber — the meter's colour has to mean the same thing on every
              // tile, and here more is better.
              bar={<Bar percent={share(running, vms.length)} tone="ok" />}
            />
            <StatTile
              label="Networks"
              href="/networking/networks"
              value={
                !networksAsked && clusterNetworks.length === 0
                  ? "—"
                  : String(clusterNetworks.length)
              }
              // Cluster-wide networks have no denominator — a cluster defines
              // as many External networks as it needs, and two of three is not
              // a proportion of anything.
              bar={<Bar percent={clusterNetworks.length > 0 ? 100 : 0} tone="ok" />}
              note={
                clusters !== null && clusters.length === 0
                  ? "No cluster yet — links are per node."
                  : "Core, Management, and External, shared by every member."
              }
            />
            <StatTile
              label="Clusters"
              href="/infrastructure/clusters"
              value={clusters === null ? "—" : String(clustersHealthy)}
              of={clusters === null ? undefined : String(clusters.length)}
              bar={<Bar percent={share(clustersHealthy, clusters?.length ?? 0)} tone="ok" />}
              note={
                clusters !== null && clusters.length === 0
                  ? "This node is not in a cluster."
                  : undefined
              }
            />
            {/* Reachable over total, so a member that is down is visible here
                rather than only in the panel below. The old tile divided the
                node count by itself and could only ever read n / n. */}
            <StatTile
              label="Nodes"
              href="/infrastructure/nodes"
              value={members === null ? "—" : String(membersOnline)}
              of={members === null ? undefined : String(members.length)}
              bar={
                <Bar
                  percent={members === null ? 0 : share(membersOnline, members.length)}
                  tone={members !== null && membersOnline < members.length ? "crit" : "ok"}
                />
              }
            />
            <StatTile
              label="Alarms"
              value={String(alarms.length)}
              // No proportion to show — an alarm count has no denominator. The
              // bar carries the worst severity instead, which is the thing
              // worth seeing from across a room.
              bar={<Bar percent={worst ? 100 : 0} tone={worst ? SEVERITY_TONE[worst] : "muted"} />}
              note={alarms.length === 0 ? "Nothing is wrong right now." : undefined}
            />
          </div>

          {/* Only when there is something to say. Nothing wrong means the page
              is tiles and panels, exactly as it is designed to be read. */}
          {alarms.length > 0 && (
            <div className="flex flex-col gap-2">
              {alarms.map((alarm) => (
                <AlarmRow key={alarm.id} alarm={alarm} />
              ))}
            </div>
          )}

          {/* Both columns stretch to the taller of the two, and the panel that
              ends each column takes up the slack — otherwise the shorter column
              stops early and the gap above Logs reads as wider than every other
              gap on the page. */}
          <div
            className="grid gap-4"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(430px, 1fr))" }}
          >
            <div className="flex flex-col gap-4 min-w-0">
              <DashPanel
                title="Top Compute Clusters"
                action={
                  clusters !== null && clusters.length > 0 ? (
                    <MoreLink href="/infrastructure/clusters">Clusters</MoreLink>
                  ) : undefined
                }
              >
                {clusters === null ? (
                  <PanelEmpty>Reading the environment…</PanelEmpty>
                ) : clusters.length === 0 ? (
                  <PanelEmpty>
                    This node is not part of a cluster. Clusters are created on Infrastructure →
                    Clusters.
                  </PanelEmpty>
                ) : (
                  <table className="qz-table">
                    <thead>
                      <tr>
                        <th style={{ width: 110 }}>Status</th>
                        <th>Name</th>
                        <th style={{ width: 80 }}>Nodes</th>
                        <th style={{ width: 130 }}>Quorum</th>
                        <th style={{ width: 100 }}>Cores</th>
                        <th style={{ width: 130 }}>RAM</th>
                        {/* What machines may be given, not what the hardware
                            is: each member's zpools count once (nothing local
                            is replicated), and the LumenFS pool contributes
                            its usable figure — already the minimum over the
                            members, so replication is charged exactly once. */}
                        <th style={{ width: 130 }}>Storage</th>
                      </tr>
                    </thead>
                    <tbody>
                      {clusters.map((cluster) => (
                        <ClusterRow
                          key={cluster.name}
                          cluster={cluster}
                          pooled={pooledCapacity(
                            inventory,
                            cluster.nodes.map((node) => node.node),
                          )}
                          // The pool is named for its cluster, and a node only
                          // serves its own cluster's pool.
                          poolUsable={
                            lumenPool && lumenPool.name === cluster.name
                              ? (lumenPool.usable_bytes ?? null)
                              : undefined
                          }
                        />
                      ))}
                    </tbody>
                  </table>
                )}
              </DashPanel>

              <DashPanel
                grow
                title="Compute Nodes"
                action={<MoreLink href="/infrastructure/nodes">Nodes</MoreLink>}
              >
                {members === null ? (
                  <PanelEmpty>Reading the nodes…</PanelEmpty>
                ) : (
                  <table className="qz-table">
                    <thead>
                      <tr>
                        <th style={{ width: 110 }}>Status</th>
                        <th>Name</th>
                        <th style={{ width: 100 }}>Machines</th>
                        <th style={{ width: 150 }}>Cores</th>
                        <th style={{ width: 170 }}>RAM</th>
                      </tr>
                    </thead>
                    <tbody>
                      {members.map((member) => (
                        <NodeRow
                          key={member.node}
                          node={member.node}
                          capacity={member.capacity}
                          reachable={member.reachable}
                        />
                      ))}
                    </tbody>
                  </table>
                )}
              </DashPanel>
            </div>

            {/* The cluster's shared networks, not this node's links: Core,
                Management, and each External network, as every member sees
                them. The per-node links realizing them are on Networking →
                Interfaces, which is a different question. */}
            {/* No `grow` needed: this panel is a grid item and already
                stretches to the row. The left column needs it because its
                panels are stacked in a flex column of their own. */}
            <DashPanel
              title="Top Networks"
              action={<MoreLink href="/networking/networks">Networks</MoreLink>}
            >
              {topNetworks.length === 0 && !networksAsked ? (
                <PanelEmpty>Reading the cluster networks…</PanelEmpty>
              ) : topNetworks.length === 0 && (clusters?.length ?? 0) === 0 ? (
                <PanelEmpty>
                  Networks are a cluster&apos;s shared definition, and this node is not in a
                  cluster. Its own links are on Networking → Interfaces.
                </PanelEmpty>
              ) : topNetworks.length === 0 ? (
                // Clusters exist but none would hand over its record. Saying so
                // is the whole job here — an empty table would read as a
                // cluster that defines no networks, which cannot happen.
                <PanelEmpty>Could not read the networks of any cluster.</PanelEmpty>
              ) : (
                <table className="qz-table">
                  <thead>
                    <tr>
                      <th style={{ width: 120 }}>Status</th>
                      <th style={{ width: 150 }}>Name</th>
                      <th style={{ width: 160 }}>Type</th>
                      <th>Subnet</th>
                    </tr>
                  </thead>
                  <tbody>
                    {topNetworks.map((row) => (
                      <NetworkRowView
                        key={row.key}
                        row={row}
                        // Whose network it is only needs saying when more than
                        // one cluster is being shown at once.
                        named={(clusters?.length ?? 0) > 1}
                      />
                    ))}
                  </tbody>
                </table>
              )}
            </DashPanel>
          </div>

          <DashPanel title="Logs">
            {tasks === null ? (
              <PanelEmpty>Reading the log…</PanelEmpty>
            ) : tasks.length === 0 ? (
              <PanelEmpty>Nothing has been done on this node yet.</PanelEmpty>
            ) : (
              <table className="qz-table">
                <thead>
                  <tr>
                    <th style={{ width: 90 }}>Status</th>
                    <th style={{ width: 180 }}>Time</th>
                    <th style={{ width: 170 }}>Machine</th>
                    <th style={{ width: 120 }}>Action</th>
                    <th style={{ width: 150 }}>User</th>
                    <th>Description</th>
                  </tr>
                </thead>
                <tbody>
                  {tasks.map((task) => (
                    <tr
                      key={task.id}
                      // The whole of a machine's history is one click away, on
                      // the machine it belongs to.
                      onClick={() =>
                        router.push(`/virtual-machines?vm=${task.vmid}&section=tasks`)
                      }
                    >
                      <td>
                        {task.status === "ok" ? (
                          <span className="badge badge-ok">OK</span>
                        ) : (
                          <span className="badge badge-crit" title={task.error ?? undefined}>
                            error
                          </span>
                        )}
                      </td>
                      <td className="mono">{formatTime(task.time)}</td>
                      <td className="mono truncate" title={nameOf(task.vmid) ?? undefined}>
                        {nameOf(task.vmid) ?? task.vmid}
                      </td>
                      <td className="mono">{task.action}</td>
                      <td className="mono">{task.user}</td>
                      <td>
                        {task.detail}
                        {task.error && <span className="qz-dim"> — {task.error}</span>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </DashPanel>
        </div>
      </PageBody>
    </Page>
  );
}

// --- rows --------------------------------------------------------------------

/// One derived alarm, in the same shape every other warning in this console
/// wears — and a link, because an alarm nobody can act on is decoration.
///
/// `.callout` top-aligns its icon and is declared outside Tailwind's layer, so
/// a utility cannot re-align it. The `mt-[1px]` on the icon is the console's
/// existing answer to that, and this uses it rather than fighting it.
function AlarmRow({ alarm }: { alarm: Alarm }) {
  const critical = alarm.severity === "critical";
  return (
    <Link href={alarm.href} className={`callout callout-${critical ? "crit" : "warn"} no-underline`}>
      <AlertTriangle
        size={17}
        className={`flex-shrink-0 mt-[1px] ${critical ? "text-[var(--qz-danger)]" : "text-[var(--qz-warn)]"}`}
      />
      <div className="flex-1 text-[13px] text-[var(--qz-fg-2)] min-w-0">{alarm.summary}</div>
      <span className="badge badge-muted flex-shrink-0">{alarm.source}</span>
    </Link>
  );
}

/// One member of the environment.
///
/// The name comes from the membership record and the figures from the node
/// itself, which is why they arrive separately: a member that is switched off
/// is still a member, and the row that says so is more use than no row at
/// all.
function NodeRow({
  node,
  capacity,
  reachable,
}: {
  node: string;
  capacity: NodeView | null;
  reachable: boolean;
}) {
  // Against what a machine may actually be given, not against every byte the
  // node has: the reserve is never available, and a meter that counts it reads
  // as healthier than the node is.
  const assignable = capacity ? assignableMemoryMib(capacity) : 0;
  return (
    <tr style={staticRow}>
      <td>
        {reachable ? (
          <Status tone="ok" label="Online" />
        ) : (
          <Status tone="crit" label="Unreachable" />
        )}
      </td>
      <td className="mono truncate" title={capacity?.hypervisor_version ?? undefined}>
        {node}
      </td>
      {capacity === null ? (
        <td colSpan={3}>
          <span className="qz-dim">Could not be asked what it is running</span>
        </td>
      ) : (
        <>
          <td className="mono">
            {capacity.running} / {capacity.machines}
          </td>
          <td>
            <Usage
              used={String(capacity.used_vcpus)}
              of={String(capacity.cpus)}
              percent={share(capacity.used_vcpus, capacity.cpus)}
            />
          </td>
          <td>
            <Usage
              used={formatMib(capacity.used_memory_mib)}
              of={formatMib(assignable)}
              percent={share(capacity.used_memory_mib, assignable)}
            />
          </td>
        </>
      )}
    </tr>
  );
}

function ClusterRow({
  cluster,
  pooled,
  poolUsable,
}: {
  cluster: ClusterView;
  pooled: PooledCapacity;
  /// The cluster's LumenFS capacity: `undefined` where it serves no pool,
  /// `null` where the pool exists but withheld its figure because a member
  /// was silent — half a pool would be a guess.
  poolUsable?: number | null;
}) {
  const online = cluster.nodes.filter((node) => node.online).length;
  // A total is only a total if every member was counted. When one could not
  // be asked, the figure is marked partial rather than quietly reported as
  // though the cluster were smaller than it is.
  const partial = pooled.counted > 0 && pooled.counted < pooled.of;
  const none = pooled.counted === 0;
  return (
    <tr style={staticRow}>
      <td>
        <Status tone={HEALTH_TONE[cluster.health]} label={HEALTH_LABEL[cluster.health]} />
      </td>
      <td className="mono truncate" title={REGIME_LABEL[cluster.regime]}>
        {cluster.name}
      </td>
      <td className="mono">
        {online} / {cluster.nodes.length}
      </td>
      <td>
        {cluster.error ? (
          <span className="qz-dim">Unknown</span>
        ) : (
          <span className="whitespace-nowrap">
            <span className="mono">
              {cluster.quorum.votes} / {cluster.quorum.expected_votes}
            </span>
            {!cluster.quorum.quorate && <span className="badge badge-crit ml-2">inquorate</span>}
          </span>
        )}
      </td>
      <Pooled partial={partial} none={none} of={pooled.of} counted={pooled.counted}>
        {pooled.usedCores} / {pooled.cores}
      </Pooled>
      <Pooled partial={partial} none={none} of={pooled.of} counted={pooled.counted}>
        {formatMib(pooled.memoryMib)}
      </Pooled>
      <Pooled
        partial={partial || poolUsable === null}
        none={none}
        of={pooled.of}
        counted={pooled.counted}
        caveat={
          poolUsable === null
            ? "The pooled storage withheld its capacity — a member is silent, so only the local pools are counted"
            : undefined
        }
      >
        {formatBytes(pooled.rawStorage + (poolUsable ?? 0))}
      </Pooled>
    </tr>
  );
}

/// A pooled figure, with the honesty about how many members it counted.
///
/// Three states, because they mean different things: a total over every
/// member, a total over some of them, and no answer at all. The middle one is
/// the one worth marking — a number that looks whole and is not is worse than
/// no number.
function Pooled({
  children,
  partial,
  none,
  counted,
  of,
  caveat,
}: {
  children: React.ReactNode;
  partial: boolean;
  none: boolean;
  counted: number;
  of: number;
  /// Replaces the members-answered sentence when the figure is partial for a
  /// different reason than an unreachable member.
  caveat?: string;
}) {
  if (none) {
    return (
      <td>
        <span className="qz-dim" title="No member could be asked">
          —
        </span>
      </td>
    );
  }
  return (
    <td className="mono whitespace-nowrap">
      {children}
      {partial && (
        <span
          className="qz-dim ml-1"
          title={caveat ?? `${counted} of ${of} members answered; the rest are not counted`}
        >
          *
        </span>
      )}
    </td>
  );
}

/// One cluster-wide network. The type cell carries the kind and the one fact
/// that tells two networks of that kind apart, the way the old interface row
/// carried a kind and its `mgmt` badge.
function NetworkRowView({ row, named }: { row: NetworkRow; named: boolean }) {
  return (
    <tr style={staticRow}>
      <td>
        <Status tone={row.tone} label={row.status} />
      </td>
      <td className="mono truncate" title={named ? `${row.cluster} - ${row.name}` : row.name}>
        {named && <span className="qz-dim">{row.cluster} - </span>}
        {row.name}
      </td>
      <td>
        <span className="whitespace-nowrap">
          {row.kind}
          {row.qualifier && <span className="badge badge-muted ml-2">{row.qualifier}</span>}
        </span>
      </td>
      <td
        className="mono truncate"
        title={[row.address, row.alsoAt].filter(Boolean).join(" - ") || undefined}
      >
        {row.address ?? <span className="qz-dim">—</span>}
        {row.alsoAt && <span className="qz-dim"> - {row.alsoAt}</span>}
      </td>
    </tr>
  );
}

/// A figure over its meter — what the compute panel's Cores and RAM cells are.
function Usage({ used, of, percent }: { used: string; of: string; percent: number }) {
  return (
    <div className="flex flex-col gap-[6px] min-w-0">
      <span className="qz-mono text-[12px] whitespace-nowrap">
        {used} <span className="text-[var(--qz-fg-4)]">/ {of}</span>
      </span>
      <Bar percent={percent} />
    </div>
  );
}

// --- helpers -----------------------------------------------------------------

/// A percentage that answers 0 rather than NaN when there is nothing to divide
/// by — a node with no processors reported is a node with an empty meter, not
/// a broken one.
const share = (part: number, whole: number): number => (whole > 0 ? (part / whole) * 100 : 0);

/// When, as an operator reads it: absolute and in their own locale, because
/// "3 minutes ago" is useless in an incident review. Matches
/// components/vm/VmTasks.tsx, which shows the same records per machine.
const formatTime = (unixSecs: number): string =>
  new Date(unixSecs * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
  });

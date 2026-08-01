"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Plus, Power, RotateCw, Trash2, Wrench, Zap } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import {
  AddNodeDialog,
  ConfirmDeadDialog,
  FenceTestDialog,
  MaintenanceDialog,
  NodePowerDialog,
} from "@/components/cluster/ClusterDialogs";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import {
  fetchEnvironment,
  nodeTone,
  removeNode,
  type ClusterNodeView,
  type EnvironmentResponse,
  type NodePowerAction,
  type UnassignedNodeView,
} from "@/lib/clusterClient";
import { fetchInventory, type InventoryResponse } from "@/lib/inventoryClient";
import { ringsByNode } from "@/lib/networkStatus";

const POLL_MS = 5000;

/// One row of the unified table: a cluster's member, or an unassigned node —
/// which is a valid standalone hypervisor, not a node in a broken state.
type NodeRow =
  | { kind: "member"; cluster: string; unreachable: boolean; node: ClusterNodeView }
  | { kind: "unassigned"; node: UnassignedNodeView };

/// Every node in the environment in one table, whatever cluster it belongs
/// to — the environment is one administrative domain, and this page is its
/// one roster.
export default function NodesPage() {
  const { setToast } = useConsole();
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  // The fencing dialogs: a guarded live test, and the break-glass confirm.
  const [fencing, setFencing] = useState<{ cluster: string; node: ClusterNodeView } | null>(null);
  const [confirming, setConfirming] = useState<{ cluster: string; node: string } | null>(null);
  const [maintaining, setMaintaining] = useState<{
    cluster: string;
    node: ClusterNodeView;
  } | null>(null);
  /// The hard power path: the node's BMC, through its fence device. Never
  /// this node — the service refuses it, and the table does not offer it.
  const [powering, setPowering] = useState<{
    node: ClusterNodeView;
    action: NodePowerAction;
  } | null>(null);

  const load = useCallback(async () => {
    try {
      setEnvironment(await fetchEnvironment());
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the environment.");
    }
    try {
      // corosync answers for the node it runs on and no other, so the
      // cluster view alone shows rings for one row. The inventory asks
      // every member for its own; a failure here just keeps the fallback.
      setInventory(await fetchInventory());
    } catch {
      /* the cluster view's local rings still show */
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // Paused while a dialog is open: a poll must not redraw the token out
    // from under the operator copying it, or the fencing dialogs mid-act.
    // The maintenance dialog polls the drain itself, which is the feed that
    // actually changes while it is open.
    if (adding || fencing || confirming || maintaining) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, adding, fencing, confirming, maintaining]);

  const remove = async (node: UnassignedNodeView) => {
    try {
      await removeNode(node.node);
      setToast(`${node.node} removed from the environment.`);
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not remove the node.");
    }
  };

  const rows = useMemo<NodeRow[]>(() => {
    if (!environment) return [];
    return [
      ...environment.clusters.flatMap((cluster) => {
        // Each member's own report of its rings, where it gave one — the
        // cluster view only ever carries the local node's.
        const rings = ringsByNode(cluster, inventory);
        return cluster.nodes.map<NodeRow>((node) => ({
          kind: "member",
          cluster: cluster.name,
          unreachable: Boolean(cluster.error),
          node: { ...node, rings: rings.get(node.node) ?? node.rings },
        }));
      }),
      ...environment.unassigned.map<NodeRow>((node) => ({ kind: "unassigned", node })),
    ];
  }, [environment, inventory]);

  return (
    <Page>
      <PageHeader
        title="Nodes"
        description="Every node this console can see, and the cluster each one belongs to."
        actions={
          <Button kind="primary" size="sm" icon={Plus} onClick={() => setAdding(true)}>
            Add Node
          </Button>
        }
      />

      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {environment === null && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the environment…</div>
          )}

          {environment !== null && (
            <NodesTable
              rows={rows}
              onRefresh={load}
              onTestFence={(cluster, node) => setFencing({ cluster, node })}
              onConfirmDead={(cluster, node) => setConfirming({ cluster, node: node.node })}
              onMaintenance={(cluster, node) => setMaintaining({ cluster, node })}
              onPower={(node, action) => setPowering({ node, action })}
              onRemove={remove}
            />
          )}
        </div>
      </PageBody>

      {adding && (
        <AddNodeDialog
          hasEnvironment={Boolean(environment?.environment)}
          onClose={() => {
            setAdding(false);
            void load();
          }}
        />
      )}

      {fencing && (
        <FenceTestDialog
          cluster={fencing.cluster}
          node={fencing.node.node}
          isLocal={fencing.node.local}
          onClose={() => {
            setFencing(null);
            void load();
          }}
          onTested={() => void load()}
        />
      )}

      {powering && (
        <NodePowerDialog
          node={powering.node}
          action={powering.action}
          onClose={() => setPowering(null)}
          onDone={(message) => {
            setPowering(null);
            setToast(message);
            void load();
          }}
        />
      )}

      {maintaining && (
        <MaintenanceDialog
          cluster={maintaining.cluster}
          node={maintaining.node.node}
          inMaintenance={Boolean(maintaining.node.maintenance)}
          onClose={() => {
            setMaintaining(null);
            void load();
          }}
          onChanged={() => void load()}
        />
      )}

      {confirming && (
        <ConfirmDeadDialog
          cluster={confirming.cluster}
          node={confirming.node}
          onClose={() => setConfirming(null)}
          onConfirmed={() => {
            setToast(
              `${confirming.node} confirmed dead — the cluster recovers as if fencing succeeded.`,
            );
            setConfirming(null);
            void load();
          }}
        />
      )}
    </Page>
  );
}

const nodeColumns: Column<NodeRow>[] = [
  {
    key: "state",
    header: "State",
    value: (row) => (row.kind === "member" ? nodeTone(row.node).label : "Standalone"),
    sortable: true,
    width: 110,
    render: (row) => {
      if (row.kind === "unassigned") {
        return <span className="badge badge-muted">Standalone</span>;
      }
      // Maintenance survives an unreachable cluster: it is Lumen's own record,
      // not something corosync was asked, and a node being worked on is a
      // common reason for the cluster to be unaskable in the first place.
      if (row.unreachable && !row.node.maintenance) {
        return <span className="badge badge-muted">Unknown</span>;
      }
      const { tone, label } = nodeTone(row.node);
      return (
        <span
          className={`badge badge-${tone}`}
          title={
            row.node.maintenance
              ? `Out of service since ${new Date(row.node.maintenance.since * 1000).toLocaleString()}, by ${row.node.maintenance.by}`
              : undefined
          }
        >
          {label}
        </span>
      );
    },
  },
  {
    key: "name",
    header: "Name",
    value: (row) => row.node.node,
    sortable: true,
    width: 180,
    render: (row) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {row.node.node}
      </span>
    ),
  },
  {
    key: "cluster",
    header: "Cluster",
    value: (row) => (row.kind === "member" ? row.cluster : ""),
    sortable: true,
    width: 140,
    render: (row) =>
      row.kind === "member" ? (
        <span className="qz-mono">{row.cluster}</span>
      ) : (
        <span className="qz-dim">Unassigned</span>
      ),
  },
  {
    key: "rings",
    header: "Rings",
    value: (row) =>
      row.kind === "member" ? row.node.rings.filter((ring) => ring.connected).length : null,
    width: 140,
    render: (row) =>
      row.kind === "unassigned" || row.node.rings.length === 0 ? (
        <Dash />
      ) : (
        <span className="inline-flex items-center gap-3">
          {row.node.rings.map((ring) => (
            <span
              key={ring.link}
              className="inline-flex items-center gap-[5px]"
              title={`ring${ring.link}: ${ring.address} — ${ring.connected ? "connected" : "disconnected"}`}
            >
              <span className={`state-dot-${ring.connected ? "ok" : "crit"}`} />
              <span className="qz-mono text-[12px] text-[var(--qz-fg-3)]">ring{ring.link}</span>
            </span>
          ))}
        </span>
      ),
  },
  {
    key: "fence",
    header: "Fencing",
    value: (row) =>
      row.kind === "member" && row.node.fence
        ? row.node.fence.failed
          ? "failing"
          : !row.node.fence.active
            ? "stopped"
            : row.node.fence.last_test
              ? "tested"
              : "untested"
        : "",
    sortable: true,
    width: 170,
    // Live state first, the recorded test second: a device whose monitor is
    // failing — or that Pacemaker has stopped after too many failures — is
    // the fact an operator acts on now, and "Tested" over a stopped device
    // was how a broken fence path read as fine. The tooltip names the IPMI
    // target, because "against which BMC" is the first debugging question.
    render: (row) => {
      if (row.kind === "unassigned" || !row.node.fence) return <Dash />;
      const fence = row.node.fence;
      const ipmi = fence.bmc_address
        ? `fence_ipmilan against ${fence.bmc_address}${
            fence.bmc_username ? ` as ${fence.bmc_username}` : ""
          }`
        : undefined;
      // The agent's own words come first when there are any: "invalid role"
      // names a firmware that stopped accepting the session, which no
      // amount of "Stopped" ever would.
      const said = fence.reason;
      if (fence.failed) {
        return (
          <span
            className="badge badge-crit"
            title={[said, ipmi && `${ipmi}.`].filter(Boolean).join(" — ") || undefined}
          >
            BMC unreachable
          </span>
        );
      }
      if (!fence.active) {
        return (
          <span
            className="badge badge-crit"
            title={
              [
                said,
                ipmi &&
                  `${ipmi} — the device is stopped, so this node cannot be fenced. ` +
                    `Fix the BMC path, then clean the resource up.`,
              ]
                .filter(Boolean)
                .join(" — ") || undefined
            }
          >
            Stopped
          </span>
        );
      }
      if (!fence.last_test) {
        return (
          <span className="badge badge-warn" title={ipmi}>
            Healthy - Untested
          </span>
        );
      }
      return (
        <span
          className={fence.last_test.passed ? "qz-dim" : "badge badge-warn"}
          title={`${ipmi ? `${ipmi} — ` : ""}last test ${new Date(
            fence.last_test.at * 1000,
          ).toLocaleString()}`}
        >
          {fence.last_test.passed ? "Healthy - Tested" : "Healthy - Test failed"}
        </span>
      );
    },
  },
  {
    key: "address",
    header: "Address",
    value: (row) => row.node.address ?? "",
    sortable: true,
    width: 140,
    render: (row) =>
      row.node.address ? <span className="qz-mono">{row.node.address}</span> : <Dash />,
  },
  {
    key: "version",
    header: "Version",
    value: (row) => row.node.controlplane_version ?? "",
    sortable: true,
    width: 100,
    render: (row) =>
      row.node.controlplane_version ? (
        <span className="qz-mono">{row.node.controlplane_version}</span>
      ) : (
        <Dash />
      ),
  },
];

function NodesTable({
  rows,
  onRefresh,
  onTestFence,
  onConfirmDead,
  onMaintenance,
  onPower,
  onRemove,
}: {
  rows: NodeRow[];
  onRefresh: () => Promise<void>;
  onTestFence: (cluster: string, node: ClusterNodeView) => void;
  onConfirmDead: (cluster: string, node: ClusterNodeView) => void;
  onMaintenance: (cluster: string, node: ClusterNodeView) => void;
  onPower: (node: ClusterNodeView, action: NodePowerAction) => void;
  onRemove: (node: UnassignedNodeView) => Promise<void>;
}) {
  // Inline confirm for removing an unassigned node, like staging a link
  // deletion: the trash flips into a Remove/Keep pair rather than opening a
  // modal for a reversible act — the node can simply join again.
  const [removing, setRemoving] = useState<string | null>(null);
  return (
    <DataTable
      rows={rows}
      columns={nodeColumns}
      rowId={(row) => row.node.node}
      storageKey="infrastructure-nodes-all"
      searchPlaceholder="Search nodes…"
      emptyMessage="No nodes yet."
      onRefresh={onRefresh}
      actionsWidth={250}
      actions={(row) =>
        row.kind === "unassigned" ? (
          row.node.local ? (
            // A node does not remove itself; the backend refuses, and the
            // control says why before it is tried.
            <span title="A node does not remove itself — do this from another environment node." />
          ) : removing === row.node.node ? (
            <span className="inline-flex items-center gap-1">
              <button
                type="button"
                className="btn btn-danger btn-sm"
                onClick={() => {
                  setRemoving(null);
                  void onRemove(row.node);
                }}
              >
                Remove
              </button>
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={() => setRemoving(null)}
              >
                Keep
              </button>
            </span>
          ) : (
            <span title="Remove from the environment">
              <Button
                kind="ghost"
                size="sm"
                icon={Trash2}
                onClick={() => setRemoving(row.node.node)}
              />
            </span>
          )
        ) : (
          <span className="inline-flex items-center gap-1">
            {/* The break-glass, offered in exactly the one state it exists
                for: lost and not successfully fenced. */}
            {!row.unreachable && row.node.unclean && (
              <button
                type="button"
                className="btn btn-danger btn-sm"
                onClick={() => onConfirmDead(row.cluster, row.node)}
              >
                Confirm dead
              </button>
            )}
            {/* On every member's row: the drain still runs on the node it is
                about — a remote row just sends the instruction to begin over
                the peer channel. Offered even when the cluster is unreachable
                if the node is already out of service, because getting *back*
                is the one thing an operator must never be locked out of. */}
            {!row.node.unclean && (!row.unreachable || row.node.maintenance) && (
              <button
                type="button"
                className="btn btn-sm"
                title={
                  row.node.maintenance
                    ? `Let Pacemaker run things on ${row.node.node} again`
                    : `Move the machines off ${row.node.node} and hold it out of service`
                }
                onClick={() => onMaintenance(row.cluster, row.node)}
              >
                <Wrench size={13} className="mr-[6px]" />
                {row.node.maintenance ? "Return to service" : "Maintenance"}
              </button>
            )}
            {!row.unreachable && !row.node.unclean && row.node.fence && (
              <span title={`Live-test the fencing of ${row.node.node}`}>
                <Button
                  kind="ghost"
                  size="sm"
                  icon={Zap}
                  onClick={() => onTestFence(row.cluster, row.node)}
                />
              </span>
            )}
            {/* The hard power path, on every row: the command goes to the
                node's BMC through a fence device, so it works when the
                node's own operating system does not. The local node cannot
                command its own BMC — the answer would go down with it — so
                its control plane relays the request through a cluster-mate's
                fence path. */}
            {row.node.fence && (
              <>
                <span title={`Power-cycle ${row.node.node} at its BMC`}>
                  <Button
                    kind="ghost"
                    size="sm"
                    icon={RotateCw}
                    onClick={() => onPower(row.node, "cycle")}
                  />
                </span>
                <span title={`Cut the power to ${row.node.node} at its BMC`}>
                  <Button
                    kind="ghost"
                    size="sm"
                    icon={Power}
                    onClick={() => onPower(row.node, "off")}
                  />
                </span>
              </>
            )}
          </span>
        )
      }
    />
  );
}

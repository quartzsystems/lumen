"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Plus, Trash2, Wrench, Zap } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import {
  AddNodeDialog,
  ConfirmDeadDialog,
  FenceTestDialog,
  MaintenanceDialog,
} from "@/components/cluster/ClusterDialogs";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import {
  fetchEnvironment,
  nodeTone,
  removeNode,
  type ClusterNodeView,
  type EnvironmentResponse,
  type UnassignedNodeView,
} from "@/lib/clusterClient";

const POLL_MS = 5000;

/// Every node in the environment: the current node first, then each cluster's
/// members, then the unassigned nodes — which are valid standalone
/// hypervisors, not nodes in a broken state.
export default function NodesPage() {
  const { setToast } = useConsole();
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  // The fencing dialogs: a guarded live test, and the break-glass confirm.
  const [fencing, setFencing] = useState<{ cluster: string; node: ClusterNodeView } | null>(null);
  const [confirming, setConfirming] = useState<{ cluster: string; node: string } | null>(null);
  const [maintaining, setMaintaining] = useState<{
    cluster: string;
    node: ClusterNodeView;
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

  // The current node's own card, wherever it lives.
  const self = useMemo(() => {
    if (!environment) return null;
    for (const cluster of environment.clusters) {
      const member = cluster.nodes.find((node) => node.local);
      if (member) return { member, cluster: cluster.name };
    }
    const unassigned = environment.unassigned.find((node) => node.local);
    return unassigned ? { member: unassigned, cluster: null } : null;
  }, [environment]);

  return (
    <Page>
      <PageHeader
        title="Nodes"
        description="Every node this console can see, grouped by the cluster it belongs to."
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

          {self && <SelfCard self={self.member} cluster={self.cluster} />}

          {environment?.clusters.map((cluster) => (
            <section key={cluster.name} className="flex flex-col gap-2">
              <h2
                className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0"
                style={{ fontFamily: "var(--qz-font-mono)" }}
              >
                {cluster.name}
              </h2>
              <MemberTable
                rows={cluster.nodes}
                unreachable={Boolean(cluster.error)}
                onRefresh={load}
                onTestFence={(node) => setFencing({ cluster: cluster.name, node })}
                onConfirmDead={(node) => setConfirming({ cluster: cluster.name, node: node.node })}
                onMaintenance={(node) => setMaintaining({ cluster: cluster.name, node })}
              />
            </section>
          ))}

          {environment !== null && environment.unassigned.length > 0 && (
            <section className="flex flex-col gap-2">
              <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
                {environment.environment ? "Unassigned" : "This node"}
              </h2>
              <UnassignedTable rows={environment.unassigned} onRefresh={load} onRemove={remove} />
            </section>
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

function SelfCard({
  self,
  cluster,
}: {
  self: ClusterNodeView | UnassignedNodeView;
  cluster: string | null;
}) {
  const rings = "rings" in self ? self.rings : [];
  const maintenance = "maintenance" in self ? self.maintenance : undefined;
  return (
    <section className="surface p-5">
      <header className="flex items-center gap-3 mb-4">
        <h2
          className="text-[15px] font-semibold text-[var(--qz-fg-1)] m-0"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {self.node}
        </h2>
        <span className="badge badge-muted">this node</span>
        {maintenance && <span className="badge badge-warn">Out of service</span>}
      </header>
      <dl className="qz-facts m-0">
        <dt>Cluster</dt>
        <dd>{cluster ?? "Unassigned — standalone hypervisor"}</dd>
        {maintenance && (
          <>
            <dt>Maintenance</dt>
            <dd>
              Since {new Date(maintenance.since * 1000).toLocaleString()}, by{" "}
              <span className="qz-mono">{maintenance.by}</span>
            </dd>
          </>
        )}
        <dt>Version</dt>
        <dd className="qz-mono">{self.controlplane_version ?? "—"}</dd>
        {rings.length > 0 && (
          <>
            <dt>Rings</dt>
            <dd>
              <span className="inline-flex items-center gap-3">
                {rings.map((ring) => (
                  <span key={ring.link} className="inline-flex items-center gap-[6px]">
                    <span className={`state-dot-${ring.connected ? "ok" : "crit"}`} />
                    <span className="qz-mono text-[12px]">
                      ring{ring.link} {ring.address}
                    </span>
                  </span>
                ))}
              </span>
            </dd>
          </>
        )}
      </dl>
    </section>
  );
}

const memberColumns = (unreachable: boolean): Column<ClusterNodeView>[] => [
  {
    key: "state",
    header: "State",
    value: (node) => nodeTone(node).label,
    sortable: true,
    width: 110,
    render: (node) => {
      // Maintenance survives an unreachable cluster: it is Lumen's own record,
      // not something corosync was asked, and a node being worked on is a
      // common reason for the cluster to be unaskable in the first place.
      if (unreachable && !node.maintenance) {
        return <span className="badge badge-muted">Unknown</span>;
      }
      const { tone, label } = nodeTone(node);
      return (
        <span
          className={`badge badge-${tone}`}
          title={
            node.maintenance
              ? `Out of service since ${new Date(node.maintenance.since * 1000).toLocaleString()}, by ${node.maintenance.by}`
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
    value: (node) => node.node,
    sortable: true,
    width: 180,
    render: (node) => (
      <span className="inline-flex items-center gap-2 min-w-0">
        <span
          className="text-[var(--qz-fg-1)] font-semibold truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {node.node}
        </span>
        {node.local && <span className="badge badge-muted">this node</span>}
      </span>
    ),
  },
  {
    key: "rings",
    header: "Rings",
    value: (node) => node.rings.filter((ring) => ring.connected).length,
    width: 140,
    render: (node) =>
      node.rings.length === 0 ? (
        <Dash />
      ) : (
        <span className="inline-flex items-center gap-3">
          {node.rings.map((ring) => (
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
    value: (node) =>
      node.fence
        ? node.fence.failed
          ? "failing"
          : !node.fence.active
            ? "stopped"
            : node.fence.last_test
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
    render: (node) => {
      if (!node.fence) return <Dash />;
      const ipmi = node.fence.bmc_address
        ? `fence_ipmilan against ${node.fence.bmc_address}${
            node.fence.bmc_username ? ` as ${node.fence.bmc_username}` : ""
          }`
        : undefined;
      if (node.fence.failed) {
        return (
          <span
            className="badge badge-crit"
            title={ipmi ? `${ipmi} — the monitor is failing.` : undefined}
          >
            BMC unreachable
          </span>
        );
      }
      if (!node.fence.active) {
        return (
          <span
            className="badge badge-crit"
            title={
              ipmi
                ? `${ipmi} — the device is stopped, so this node cannot be fenced. ` +
                  `Fix the BMC path, then clean the resource up.`
                : undefined
            }
          >
            Stopped
          </span>
        );
      }
      if (!node.fence.last_test) {
        return (
          <span className="badge badge-warn" title={ipmi}>
            Healthy · untested
          </span>
        );
      }
      return (
        <span
          className={node.fence.last_test.passed ? "qz-dim" : "badge badge-warn"}
          title={`${ipmi ? `${ipmi} — ` : ""}last test ${new Date(
            node.fence.last_test.at * 1000,
          ).toLocaleString()}`}
        >
          {node.fence.last_test.passed ? "Healthy · tested" : "Healthy · test failed"}
        </span>
      );
    },
  },
  {
    key: "address",
    header: "Address",
    value: (node) => node.address ?? "",
    sortable: true,
    width: 140,
    render: (node) => (node.address ? <span className="qz-mono">{node.address}</span> : <Dash />),
  },
  {
    key: "version",
    header: "Version",
    value: (node) => node.controlplane_version ?? "",
    sortable: true,
    width: 100,
    render: (node) =>
      node.controlplane_version ? (
        <span className="qz-mono">{node.controlplane_version}</span>
      ) : (
        <Dash />
      ),
  },
];

function MemberTable({
  rows,
  unreachable,
  onRefresh,
  onTestFence,
  onConfirmDead,
  onMaintenance,
}: {
  rows: ClusterNodeView[];
  unreachable: boolean;
  onRefresh: () => Promise<void>;
  onTestFence: (node: ClusterNodeView) => void;
  onConfirmDead: (node: ClusterNodeView) => void;
  onMaintenance: (node: ClusterNodeView) => void;
}) {
  const columns = useMemo(() => memberColumns(unreachable), [unreachable]);
  return (
    <DataTable
      rows={rows}
      columns={columns}
      rowId={(node) => node.node}
      storageKey="infrastructure-nodes"
      searchPlaceholder="Search nodes…"
      emptyMessage="No members."
      onRefresh={onRefresh}
      actionsWidth={190}
      actions={(node) => (
        <span className="inline-flex items-center gap-1">
          {/* The break-glass, offered in exactly the one state it exists
              for: lost and not successfully fenced. */}
          {!unreachable && node.unclean && (
            <button
              type="button"
              className="btn btn-danger btn-sm"
              onClick={() => onConfirmDead(node)}
            >
              Confirm dead
            </button>
          )}
          {/* Only on this node's own row: a drain moves machines, and only
              the node running them can move them. Offered even when the
              cluster is unreachable if the node is already out of service,
              because getting *back* is the one thing an operator must never
              be locked out of. */}
          {node.local && !node.unclean && (!unreachable || node.maintenance) && (
            <button
              type="button"
              className="btn btn-sm"
              title={
                node.maintenance
                  ? `Let Pacemaker run things on ${node.node} again`
                  : `Move the machines off ${node.node} and hold it out of service`
              }
              onClick={() => onMaintenance(node)}
            >
              <Wrench size={13} className="mr-[6px]" />
              {node.maintenance ? "Return to service" : "Maintenance"}
            </button>
          )}
          {!unreachable && !node.unclean && node.fence && (
            <span title={`Live-test the fencing of ${node.node}`}>
              <Button kind="ghost" size="sm" icon={Zap} onClick={() => onTestFence(node)} />
            </span>
          )}
        </span>
      )}
    />
  );
}

const unassignedColumns: Column<UnassignedNodeView>[] = [
  {
    key: "name",
    header: "Name",
    value: (node) => node.node,
    sortable: true,
    width: 200,
    render: (node) => (
      <span className="inline-flex items-center gap-2 min-w-0">
        <span
          className="text-[var(--qz-fg-1)] font-semibold truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {node.node}
        </span>
        {node.local && <span className="badge badge-muted">this node</span>}
      </span>
    ),
  },
  {
    key: "address",
    header: "Address",
    value: (node) => node.address ?? "",
    sortable: true,
    width: 160,
    render: (node) => (node.address ? <span className="qz-mono">{node.address}</span> : <Dash />),
  },
  {
    key: "version",
    header: "Version",
    value: (node) => node.controlplane_version ?? "",
    sortable: true,
    width: 110,
    render: (node) =>
      node.controlplane_version ? (
        <span className="qz-mono">{node.controlplane_version}</span>
      ) : (
        <Dash />
      ),
  },
  {
    key: "role",
    header: "Role",
    value: () => "standalone",
    render: () => <span className="qz-dim">Standalone hypervisor</span>,
  },
];

function UnassignedTable({
  rows,
  onRefresh,
  onRemove,
}: {
  rows: UnassignedNodeView[];
  onRefresh: () => Promise<void>;
  onRemove: (node: UnassignedNodeView) => Promise<void>;
}) {
  // Inline confirm, like staging a link deletion: the trash flips into a
  // ✓/✕ pair rather than opening a modal for a reversible act — the node
  // can simply join again.
  const [confirming, setConfirming] = useState<string | null>(null);
  return (
    <DataTable
      rows={rows}
      columns={unassignedColumns}
      rowId={(node) => node.node}
      storageKey="infrastructure-nodes-unassigned"
      searchPlaceholder="Search nodes…"
      emptyMessage="Every node is in a cluster."
      onRefresh={onRefresh}
      actionsWidth={96}
      actions={(node) =>
        node.local ? (
          // A node does not remove itself; the backend refuses, and the
          // control says why before it is tried.
          <span title="A node does not remove itself — do this from another environment node." />
        ) : confirming === node.node ? (
          <span className="inline-flex items-center gap-1">
            <button
              type="button"
              className="btn btn-danger btn-sm"
              onClick={() => {
                setConfirming(null);
                void onRemove(node);
              }}
            >
              Remove
            </button>
            <button
              type="button"
              className="btn btn-ghost btn-sm"
              onClick={() => setConfirming(null)}
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
              onClick={() => setConfirming(node.node)}
            />
          </span>
        )
      }
    />
  );
}

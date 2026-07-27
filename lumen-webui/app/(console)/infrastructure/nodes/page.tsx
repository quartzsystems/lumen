"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Plus } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { ApiError } from "@/lib/authClient";
import {
  fetchEnvironment,
  nodeTone,
  type ClusterNodeView,
  type EnvironmentResponse,
  type UnassignedNodeView,
} from "@/lib/clusterClient";

const POLL_MS = 5000;

/// Every node in the environment: the current node first, then each cluster's
/// members, then the unassigned nodes — which are valid standalone
/// hypervisors, not nodes in a broken state.
export default function NodesPage() {
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

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
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

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
          // The join-token flow lands with a later stage.
          <span title="The environment join workflow has not landed yet.">
            <Button kind="primary" size="sm" icon={Plus} disabled>
              Add Node
            </Button>
          </span>
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
              <MemberTable rows={cluster.nodes} unreachable={Boolean(cluster.error)} onRefresh={load} />
            </section>
          ))}

          {environment !== null && environment.unassigned.length > 0 && (
            <section className="flex flex-col gap-2">
              <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
                {environment.environment ? "Unassigned" : "This node"}
              </h2>
              <UnassignedTable rows={environment.unassigned} onRefresh={load} />
            </section>
          )}
        </div>
      </PageBody>
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
      </header>
      <dl className="qz-facts m-0">
        <dt>Cluster</dt>
        <dd>{cluster ?? "Unassigned — standalone hypervisor"}</dd>
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
      if (unreachable) return <span className="badge badge-muted">Unknown</span>;
      const { tone, label } = nodeTone(node);
      return <span className={`badge badge-${tone}`}>{label}</span>;
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
      node.fence ? (node.fence.failed ? "failing" : node.fence.last_test ? "tested" : "untested") : "",
    sortable: true,
    width: 150,
    render: (node) => {
      if (!node.fence) return <Dash />;
      if (node.fence.failed) return <span className="badge badge-crit">BMC unreachable</span>;
      if (!node.fence.last_test) return <span className="badge badge-warn">Untested</span>;
      return (
        <span
          className="qz-dim"
          title={`Last test ${new Date(node.fence.last_test.at * 1000).toLocaleString()}`}
        >
          {node.fence.last_test.passed ? "Tested" : "Test failed"}
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
}: {
  rows: ClusterNodeView[];
  unreachable: boolean;
  onRefresh: () => Promise<void>;
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
}: {
  rows: UnassignedNodeView[];
  onRefresh: () => Promise<void>;
}) {
  return (
    <DataTable
      rows={rows}
      columns={unassignedColumns}
      rowId={(node) => node.node}
      storageKey="infrastructure-nodes-unassigned"
      searchPlaceholder="Search nodes…"
      emptyMessage="Every node is in a cluster."
      onRefresh={onRefresh}
    />
  );
}

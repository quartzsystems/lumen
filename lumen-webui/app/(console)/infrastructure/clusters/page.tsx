"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Plus, Trash2, UserPlus } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { AddNodeDialog } from "@/components/cluster/AddNodeDialog";
import { CreateClusterDialog } from "@/components/cluster/CreateClusterDialog";
import { DestroyClusterDialog } from "@/components/cluster/ClusterDialogs";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import {
  fetchEnvironment,
  HEALTH_LABEL,
  HEALTH_TONE,
  nodeTone,
  REGIME_LABEL,
  type ClusterView,
  type EnvironmentResponse,
} from "@/lib/clusterClient";

const POLL_MS = 5000;

/// Every cluster in the environment — the plural is the point: this console
/// administers all of them, from any node. A cluster is an independent
/// quorum/fencing/replication domain of 2–5 nodes; the environment above it is
/// administration only and never a data path.
export default function ClustersPage() {
  const { setToast } = useConsole();
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState<ClusterView | null>(null);
  const [addingTo, setAddingTo] = useState<ClusterView | null>(null);

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
    // Polling pauses while a dialog is open, so a refresh cannot move a
    // wizard's pickers out from under the operator mid-choice.
    if (creating || destroying || addingTo) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, creating, destroying, addingTo]);

  const noEnvironment = environment !== null && !environment.environment;
  const spareNodes = environment?.unassigned.length ?? 0;
  const createBlocked = noEnvironment
    ? "Join or bootstrap an environment first — Infrastructure → Nodes → Add Node."
    : spareNodes < 2
      ? "A cluster needs at least two unassigned environment nodes."
      : null;

  return (
    <Page>
      <PageHeader
        title="Clusters"
        description="Every cluster in this environment: quorum, membership, and fencing at a glance."
        actions={
          // A disabled control explains itself rather than being silently
          // grey.
          <span title={createBlocked ?? undefined}>
            <Button
              kind="primary"
              size="sm"
              icon={Plus}
              disabled={createBlocked !== null}
              onClick={() => setCreating(true)}
            >
              Create Cluster
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

          {noEnvironment && (
            <div className="surface px-6 py-12 text-center">
              <div className="text-[14px] font-semibold text-[var(--qz-fg-2)]">
                This node is not part of an environment
              </div>
              <p className="text-[13px] text-[var(--qz-fg-4)] mt-2 mb-0 max-w-[520px] mx-auto">
                It keeps working as a standalone hypervisor. An environment is one administrative
                domain over several nodes — one sign-in, one console, every node visible — and
                clusters are built inside it from two to five nodes each. Start on
                Infrastructure → Nodes → Add Node.
              </p>
            </div>
          )}

          {environment !== null &&
            environment.environment &&
            environment.clusters.length === 0 && (
              <div className="surface px-6 py-12 text-center">
                <div className="text-[14px] font-semibold text-[var(--qz-fg-2)]">
                  No clusters yet
                </div>
                <p className="text-[13px] text-[var(--qz-fg-4)] mt-2 mb-0">
                  The environment has {environment.environment.nodes} node
                  {environment.environment.nodes === 1 ? "" : "s"}, all unassigned. A cluster is
                  built from 2–5 of them.
                </p>
              </div>
            )}

          {environment !== null && environment.clusters.length > 0 && (
            <ClusterTable
              rows={environment.clusters}
              spareNodes={spareNodes}
              onRefresh={load}
              onAddNode={setAddingTo}
              onDestroy={setDestroying}
            />
          )}
        </div>
      </PageBody>

      {creating && environment && (
        <CreateClusterDialog
          unassigned={environment.unassigned}
          onClose={() => setCreating(false)}
          onCreated={() => {
            setToast("Cluster created.");
            void load();
          }}
        />
      )}

      {addingTo && environment && (
        <AddNodeDialog
          cluster={addingTo}
          unassigned={environment.unassigned}
          onClose={() => setAddingTo(null)}
          onAdded={() => {
            setToast(`${addingTo.name} grew by a node.`);
            void load();
          }}
        />
      )}

      {destroying && (
        <DestroyClusterDialog
          cluster={destroying}
          onClose={() => setDestroying(null)}
          onDestroyed={() => {
            setToast(`${destroying.name} destroyed — its nodes are unassigned again.`);
            setDestroying(null);
            void load();
          }}
        />
      )}
    </Page>
  );
}

const clusterColumns: Column<ClusterView>[] = [
  {
    key: "health",
    header: "Health",
    value: (cluster) => HEALTH_LABEL[cluster.health],
    sortable: true,
    width: 110,
    // An unreachable cluster's badge carries the why: its row shows dashes
    // for everything corosync would have answered, and this is the one place
    // left to say what stopped it answering.
    render: (cluster) => (
      <span className={`badge badge-${HEALTH_TONE[cluster.health]}`} title={cluster.error}>
        {HEALTH_LABEL[cluster.health]}
      </span>
    ),
  },
  {
    key: "name",
    header: "Name",
    value: (cluster) => cluster.name,
    sortable: true,
    width: 170,
    render: (cluster) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {cluster.name}
      </span>
    ),
  },
  {
    key: "regime",
    header: "Regime",
    value: (cluster) => REGIME_LABEL[cluster.regime],
    sortable: true,
    width: 210,
    render: (cluster) => (
      <span className="truncate">
        {REGIME_LABEL[cluster.regime]}
        {cluster.preferred_node && (
          <span className="qz-mono text-[12px] text-[var(--qz-fg-4)]">
            {" "}
            · prefers {cluster.preferred_node}
          </span>
        )}
      </span>
    ),
  },
  {
    key: "nodes",
    header: "Nodes",
    value: (cluster) => cluster.nodes.filter((node) => node.online).length,
    sortable: true,
    width: 180,
    // Membership stays at a glance: one dot per member, named in its
    // tooltip. The per-node detail lives on Infrastructure → Nodes.
    render: (cluster) => {
      const online = cluster.nodes.filter((node) => node.online).length;
      return (
        <span className="inline-flex items-center gap-2">
          <span className="inline-flex items-center gap-[5px]">
            {cluster.nodes.map((node) => {
              const { tone, label } = nodeTone(node);
              return (
                <span
                  key={node.node}
                  className={`state-dot-${cluster.error ? "muted" : tone}`}
                  title={`${node.node} — ${cluster.error ? "unknown" : label}`}
                />
              );
            })}
          </span>
          <span className="qz-dim">
            {online} of {cluster.nodes.length} online
          </span>
        </span>
      );
    },
  },
  {
    key: "quorum",
    header: "Quorum",
    value: (cluster) =>
      cluster.error ? "" : cluster.quorum.quorate ? "Quorate" : "Not quorate",
    sortable: true,
    width: 170,
    render: (cluster) =>
      cluster.error ? (
        <Dash />
      ) : (
        <span>
          {cluster.quorum.quorate ? "Quorate" : "Not quorate"}
          <span className="qz-mono text-[12px] text-[var(--qz-fg-4)]">
            {" "}
            ({cluster.quorum.votes}/{cluster.quorum.expected_votes} votes)
          </span>
        </span>
      ),
  },
  {
    key: "fence",
    header: "Fencing",
    value: (cluster) =>
      cluster.error
        ? ""
        : cluster.fence.devices === 0
          ? "not configured"
          : cluster.fence.failed > 0
            ? "failing"
            : cluster.fence.untested > 0
              ? "untested"
              : "healthy",
    sortable: true,
    width: 170,
    // IPMI is the only fence path this appliance has, so an untested
    // direction stays a pinned warning until the test is run — the card's
    // callout, folded into the badge and its tooltip.
    render: (cluster) => {
      if (cluster.error) return <Dash />;
      if (cluster.fence.devices === 0) {
        return <span className="badge badge-warn">Not configured yet</span>;
      }
      if (cluster.fence.failed > 0) {
        return (
          <span className="badge badge-crit">
            {cluster.fence.failed} device{cluster.fence.failed === 1 ? "" : "s"} failing
          </span>
        );
      }
      if (cluster.fence.untested > 0) {
        return (
          <span
            className="badge badge-warn"
            title={`Fencing has not been live-tested for ${cluster.fence.untested} node${
              cluster.fence.untested === 1 ? "" : "s"
            }. An untested fence path is one that fails during the outage that needed it — test each direction from Infrastructure → Nodes.`}
          >
            {cluster.fence.untested} untested
          </span>
        );
      }
      return (
        <span>
          {cluster.fence.healthy} of {cluster.fence.devices} healthy
        </span>
      );
    },
  },
];

function ClusterTable({
  rows,
  spareNodes,
  onRefresh,
  onAddNode,
  onDestroy,
}: {
  rows: ClusterView[];
  spareNodes: number;
  onRefresh: () => Promise<void>;
  onAddNode: (cluster: ClusterView) => void;
  onDestroy: (cluster: ClusterView) => void;
}) {
  return (
    <DataTable
      rows={rows}
      columns={clusterColumns}
      rowId={(cluster) => cluster.name}
      storageKey="infrastructure-clusters"
      searchPlaceholder="Search clusters…"
      emptyMessage="No clusters."
      onRefresh={onRefresh}
      actionsWidth={96}
      actions={(cluster) => (
        <span className="inline-flex items-center gap-1">
          {cluster.nodes.length < 5 && spareNodes > 0 && (
            <span title="Add a node to this cluster">
              <Button kind="ghost" size="sm" icon={UserPlus} onClick={() => onAddNode(cluster)} />
            </span>
          )}
          <Button kind="ghost" size="sm" icon={Trash2} onClick={() => onDestroy(cluster)} />
        </span>
      )}
    />
  );
}

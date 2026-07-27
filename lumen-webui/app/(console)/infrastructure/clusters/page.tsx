"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Plus, ShieldAlert, Trash2, UserPlus } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
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
            <div
              className="grid gap-4"
              style={{ gridTemplateColumns: "repeat(auto-fill, minmax(340px, 1fr))" }}
            >
              {environment.clusters.map((cluster) => (
                <ClusterCard
                  key={cluster.name}
                  cluster={cluster}
                  canGrow={cluster.nodes.length < 5 && spareNodes > 0}
                  onAddNode={() => setAddingTo(cluster)}
                  onDestroy={() => setDestroying(cluster)}
                />
              ))}
            </div>
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

function ClusterCard({
  cluster,
  canGrow,
  onAddNode,
  onDestroy,
}: {
  cluster: ClusterView;
  canGrow: boolean;
  onAddNode: () => void;
  onDestroy: () => void;
}) {
  const online = cluster.nodes.filter((node) => node.online).length;
  return (
    <section className="surface p-5 flex flex-col gap-4">
      <header className="flex items-center gap-3">
        <h2
          className="text-[15px] font-semibold text-[var(--qz-fg-1)] m-0 flex-1 truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {cluster.name}
        </h2>
        <span className={`badge badge-${HEALTH_TONE[cluster.health]}`}>
          {HEALTH_LABEL[cluster.health]}
        </span>
        {canGrow && (
          <span title="Add a node to this cluster">
            <Button kind="ghost" size="sm" icon={UserPlus} onClick={onAddNode} />
          </span>
        )}
        <Button kind="ghost" size="sm" icon={Trash2} onClick={onDestroy} />
      </header>

      {cluster.error && <div className="text-[12px] text-[var(--qz-fg-4)]">{cluster.error}</div>}

      <dl className="qz-facts m-0">
        <dt>Regime</dt>
        <dd>
          {REGIME_LABEL[cluster.regime]}
          {cluster.preferred_node && (
            <span className="qz-mono text-[12px] text-[var(--qz-fg-4)]">
              {" "}
              · prefers {cluster.preferred_node}
            </span>
          )}
        </dd>
        <dt>Nodes</dt>
        <dd>
          {online} of {cluster.nodes.length} online
        </dd>
        <dt>Quorum</dt>
        <dd>
          {cluster.error ? (
            "—"
          ) : (
            <>
              {cluster.quorum.quorate ? "Quorate" : "Not quorate"}
              <span className="qz-mono text-[12px] text-[var(--qz-fg-4)]">
                {" "}
                ({cluster.quorum.votes}/{cluster.quorum.expected_votes} votes)
              </span>
            </>
          )}
        </dd>
        <dt>Replication</dt>
        {/* Replicated volumes land with a later stage; a dash is honest. */}
        <dd>—</dd>
        <dt>Fencing</dt>
        <dd>
          {cluster.error ? (
            "—"
          ) : cluster.fence.devices === 0 ? (
            <span className="badge badge-warn">Not configured yet</span>
          ) : cluster.fence.failed > 0 ? (
            <span className="badge badge-crit">
              {cluster.fence.failed} device{cluster.fence.failed === 1 ? "" : "s"} failing
            </span>
          ) : (
            <span>
              {cluster.fence.healthy} of {cluster.fence.devices} healthy
            </span>
          )}
        </dd>
      </dl>

      {/* IPMI is the only fence path this appliance has, so an untested
          direction is a warning that stays pinned until the test is run. */}
      {!cluster.error && cluster.fence.untested > 0 && (
        <div className="callout callout-warn">
          <ShieldAlert size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            Fencing has not been live-tested for {cluster.fence.untested} node
            {cluster.fence.untested === 1 ? "" : "s"}. An untested fence path is one that fails
            during the outage that needed it — test each direction from Infrastructure → Nodes.
          </div>
        </div>
      )}

      <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
        {cluster.nodes.map((node) => {
          const { tone, label } = nodeTone(node);
          return (
            <li key={node.node} className="flex items-center gap-2 text-[13px]">
              <span className={`state-dot-${cluster.error ? "muted" : tone}`} />
              <span className="qz-mono text-[var(--qz-fg-2)]">{node.node}</span>
              {node.local && <span className="badge badge-muted">this node</span>}
              <span className="text-[12px] text-[var(--qz-fg-4)] ml-auto">
                {cluster.error ? "unknown" : label}
              </span>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

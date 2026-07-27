"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Plus, ShieldAlert } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { Button } from "@/components/ui/Button";
import { ApiError } from "@/lib/authClient";
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

  const noEnvironment = environment !== null && !environment.environment;

  return (
    <Page>
      <PageHeader
        title="Clusters"
        description="Every cluster in this environment: quorum, membership, and fencing at a glance."
        actions={
          // The create wizard lands with a later stage; until then the
          // control says why it is grey rather than being silently absent.
          <span title="Cluster creation has not landed yet.">
            <Button kind="primary" size="sm" icon={Plus} disabled>
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
                clusters are built inside it from two to five nodes each.
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
                <ClusterCard key={cluster.name} cluster={cluster} />
              ))}
            </div>
          )}
        </div>
      </PageBody>
    </Page>
  );
}

function ClusterCard({ cluster }: { cluster: ClusterView }) {
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
      </header>

      {cluster.error && <div className="text-[12px] text-[var(--qz-fg-4)]">{cluster.error}</div>}

      <dl className="qz-facts m-0">
        <dt>Regime</dt>
        <dd>{REGIME_LABEL[cluster.regime]}</dd>
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
            during the outage that needed it.
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

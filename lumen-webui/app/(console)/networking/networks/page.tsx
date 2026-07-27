"use client";

import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { AlertTriangle, ArrowRight } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { ApiError } from "@/lib/authClient";
import {
  fetchClusterNetworks,
  fetchEnvironment,
  type AddressedMember,
  type ClusterNetworks,
  type ClusterView,
  type EnvironmentResponse,
  type ExternalNetwork,
} from "@/lib/clusterClient";

const POLL_MS = 5000;

/// What one cluster's networks request answered: the document, or the reason
/// there is none. Kept per cluster so one unreachable record does not blank
/// the others.
type NetworksAnswer = { networks: ClusterNetworks } | { error: string };

/// The three typed networks every cluster's members share — Core, Management,
/// External — read off the replicated record and joined with what corosync
/// reports about each ring. This page presents the shared definition; the
/// per-node links realizing it live on Networking → Interfaces.
export default function NetworksPage() {
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [answers, setAnswers] = useState<Record<string, NetworksAnswer>>({});
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const env = await fetchEnvironment();
      const entries = await Promise.all(
        env.clusters.map(async (cluster): Promise<[string, NetworksAnswer]> => {
          try {
            return [cluster.name, { networks: await fetchClusterNetworks(cluster.name) }];
          } catch (err) {
            return [
              cluster.name,
              { error: err instanceof Error ? err.message : "Could not read the networks." },
            ];
          }
        }),
      );
      setEnvironment(env);
      setAnswers(Object.fromEntries(entries));
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the environment.");
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const noEnvironment = environment !== null && !environment.environment;
  const clusters = environment?.clusters ?? [];

  return (
    <Page>
      <PageHeader
        title="Networks"
        description="The cluster-wide networks every member shares: Core, Management, and External."
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

          {noEnvironment && <NoNetworksYet reason="standalone" />}

          {environment !== null && environment.environment && clusters.length === 0 && (
            <NoNetworksYet reason="no-cluster" />
          )}

          {clusters.map((cluster) => (
            <ClusterNetworksSection
              key={cluster.name}
              cluster={cluster}
              named={clusters.length > 1}
              answer={answers[cluster.name] ?? null}
            />
          ))}

          {environment !== null &&
            environment.environment &&
            clusters.length > 0 &&
            environment.unassigned.length > 0 && (
              <p className="text-[13px] text-[var(--qz-fg-4)] m-0">
                {environment.unassigned.length} environment node
                {environment.unassigned.length === 1 ? " is" : "s are"} not in a cluster and
                {environment.unassigned.length === 1 ? " has" : " have"} only node-local
                networking — see{" "}
                <Link
                  href="/networking/interfaces"
                  className="text-[var(--qz-accent)] no-underline"
                >
                  Interfaces
                </Link>
                .
              </p>
            )}
        </div>
      </PageBody>
    </Page>
  );
}

/// The typed networks exist once a cluster does; before that, this page has
/// nothing to claim — and says so instead of dressing up node-local links as
/// cluster networks.
function NoNetworksYet({ reason }: { reason: "standalone" | "no-cluster" }) {
  return (
    <div className="surface px-6 py-12 text-center">
      <div className="text-[14px] font-semibold text-[var(--qz-fg-2)]">
        {reason === "standalone"
          ? "This node is not part of an environment"
          : "No clusters yet"}
      </div>
      <p className="text-[13px] text-[var(--qz-fg-4)] mt-2 mb-0 max-w-[560px] mx-auto">
        Networks are a cluster&apos;s shared definition — Core carries storage replication and the
        cluster heartbeat, Management carries the console, External carries virtual machine
        traffic. They are defined when a cluster is created, on
        Infrastructure → Clusters. Until then, links are configured per node on
        Networking → Interfaces.
      </p>
    </div>
  );
}

function ClusterNetworksSection({
  cluster,
  named,
  answer,
}: {
  cluster: ClusterView;
  /// Whether the environment has more than one cluster — only then does the
  /// section need saying whose networks these are.
  named: boolean;
  answer: NetworksAnswer | null;
}) {
  return (
    <section className="flex flex-col gap-4">
      {named && (
        <h2
          className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0"
          style={{ fontFamily: "var(--qz-font-mono)" }}
        >
          {cluster.name}
        </h2>
      )}

      {answer === null && (
        <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the networks…</div>
      )}

      {answer !== null && "error" in answer && (
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{answer.error}</div>
        </div>
      )}

      {answer !== null && "networks" in answer && (
        <>
          <div
            className="grid gap-4"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(340px, 1fr))" }}
          >
            <NetworkCard
              title="Core"
              subtitle="Storage replication and the cluster heartbeat, ring 0."
              facts={[
                { label: "Subnet", value: answer.networks.core.subnet },
                { label: "MTU", value: String(answer.networks.core.mtu) },
              ]}
              members={answer.networks.core.members}
              ring={0}
              cluster={cluster}
            />
            <NetworkCard
              title="Management"
              subtitle="The console and the heartbeat's second ring."
              facts={[
                { label: "Subnet", value: answer.networks.management.subnet },
                { label: "VIP", value: answer.networks.management.vip ?? "—" },
              ]}
              members={answer.networks.management.members}
              ring={1}
              cluster={cluster}
            />
          </div>
          <ExternalCard external={answer.networks.external} />
        </>
      )}
    </section>
  );
}

/// One addressed network — Core or Management — as a card: the shared facts,
/// then each member's seat with what corosync says about that member's ring.
function NetworkCard({
  title,
  subtitle,
  facts,
  members,
  ring,
  cluster,
}: {
  title: string;
  subtitle: string;
  facts: { label: string; value: string }[];
  members: AddressedMember[];
  ring: number;
  cluster: ClusterView;
}) {
  return (
    <section className="surface p-5 flex flex-col gap-4">
      <header>
        <h3 className="text-[14px] font-semibold text-[var(--qz-fg-1)] m-0">{title}</h3>
        <p className="text-[12px] text-[var(--qz-fg-4)] mt-1 mb-0">{subtitle}</p>
      </header>

      <dl className="qz-facts m-0">
        {facts.map((fact) => (
          <FactRow key={fact.label} label={fact.label} value={fact.value} />
        ))}
      </dl>

      <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
        {members.map((member) => (
          <MemberRow key={member.node} member={member} ring={ring} cluster={cluster} />
        ))}
      </ul>
    </section>
  );
}

function FactRow({ label, value }: { label: string; value: string }) {
  return (
    <>
      <dt>{label}</dt>
      <dd className="qz-mono">{value}</dd>
    </>
  );
}

/// One member's seat: which link carries the network there, on what address,
/// and whether corosync sees that member's ring — live state against the
/// definition, from the one place that already watches it.
function MemberRow({
  member,
  ring,
  cluster,
}: {
  member: AddressedMember;
  ring: number;
  cluster: ClusterView;
}) {
  const node = cluster.nodes.find((candidate) => candidate.node === member.node) ?? null;
  const link = node?.rings.find((candidate) => candidate.link === ring) ?? null;

  // The cluster not answering is "unknown", not "down": nothing here may
  // claim a state nobody observed.
  const state =
    cluster.error || node === null || link === null
      ? ({ tone: "muted", label: "Unknown" } as const)
      : link.connected
        ? ({ tone: "ok", label: "Connected" } as const)
        : ({ tone: "crit", label: "Down" } as const);

  return (
    <li className="flex items-center gap-2 text-[13px]">
      <span className={`state-dot-${state.tone}`} title={`ring ${ring}: ${state.label}`} />
      <span className="qz-mono text-[var(--qz-fg-2)]">{member.node}</span>
      {node?.local && <span className="badge badge-muted">this node</span>}
      <span className="qz-mono text-[12px] text-[var(--qz-fg-4)] ml-auto">
        {member.interface} · {member.address}
      </span>
    </li>
  );
}

/// The VLAN semantics, spelled the way the definition means them.
const vlanText = (network: ExternalNetwork): string =>
  network.mode === "trunk"
    ? network.allowed.length > 0
      ? `Trunk — VLANs ${network.allowed.join(", ")}`
      : "Trunk"
    : `Access — VLAN ${network.vlan}`;

/// External networks: zero or more, and zero is the common state until
/// realization lands — so the empty case is a sentence, not an empty table.
function ExternalCard({ external }: { external: ExternalNetwork[] }) {
  return (
    <section className="surface p-5 flex flex-col gap-4">
      <header>
        <h3 className="text-[14px] font-semibold text-[var(--qz-fg-1)] m-0">External</h3>
        <p className="text-[12px] text-[var(--qz-fg-4)] mt-1 mb-0">
          Virtual machine traffic: an identically named bridge on every member, no host
          addressing.
        </p>
      </header>

      {external.length === 0 ? (
        <p className="text-[13px] text-[var(--qz-fg-4)] m-0">
          This cluster defines no External networks yet. Virtual machines attach to node-local
          bridges meanwhile —{" "}
          <Link
            href="/networking/interfaces"
            className="text-[var(--qz-accent)] no-underline inline-flex items-center gap-1"
          >
            Interfaces
            <ArrowRight size={13} />
          </Link>
        </p>
      ) : (
        <ul className="m-0 p-0 flex flex-col gap-3" style={{ listStyle: "none" }}>
          {external.map((network) => (
            <li key={network.name} className="flex flex-wrap items-center gap-x-6 gap-y-2">
              <span className="qz-mono text-[13px] font-semibold text-[var(--qz-fg-1)]">
                {network.name}
              </span>
              <span className="qz-mono text-[13px] text-[var(--qz-fg-3)]">{network.bridge}</span>
              <span className="text-[13px] text-[var(--qz-fg-3)]">{vlanText(network)}</span>
              <span className="qz-mono text-[12px] text-[var(--qz-fg-4)] ml-auto">
                {network.uplinks
                  .map((uplink) => `${uplink.node}: ${uplink.interface}`)
                  .join(" · ")}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

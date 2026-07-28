"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { AlertTriangle, Plus } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Status } from "@/components/dashboard/DashboardBits";
import { Button } from "@/components/ui/Button";
import { ApiError } from "@/lib/authClient";
import {
  fetchClusterNetworks,
  fetchEnvironment,
  type ClusterNetworks,
  type ClusterView,
  type EnvironmentResponse,
  type ExternalNetwork,
} from "@/lib/clusterClient";
import { externalState, ringState, vipState, type StatusTone } from "@/lib/networkStatus";
import { fetchInventory, type InventoryResponse } from "@/lib/inventoryClient";
import { useConsole } from "@/lib/ConsoleContext";
import { CreateExternalNetworkDialog } from "@/components/network/CreateExternalNetworkDialog";

const POLL_MS = 5000;

/// What one cluster's networks request answered: the document, or the reason
/// there is none. Kept per cluster so one unreachable record does not blank
/// the others.
type NetworksAnswer = { networks: ClusterNetworks } | { error: string };

/// One row of the table: a cluster-wide network of one of the three types.
///
/// Flat rather than three shapes, because the table wants one set of columns.
/// The fields a given type does not have are null and render as a dash, which
/// is more honest than a column that means something different per row.
interface NetworkRow {
  key: string;
  cluster: string;
  name: string;
  kind: "Core" | "Management" | "External";
  /// Host addressing, or null for External — a network with no address on the
  /// host is the whole point of that type.
  subnet: string | null;
  /// The cluster VIP, the only second address any of these has.
  vip: string | null;
  /// MTU for Core, VLAN semantics for External, nothing for Management.
  detail: string | null;
  tone: StatusTone;
  status: string;
  /// Which members carry it, for the column that answers "is this everywhere?"
  members: string[];
  of: number;
}

/// The rows one cluster contributes: Core, Management, and each External
/// network it defines.
function networkRows(cluster: ClusterView, networks: ClusterNetworks): NetworkRow[] {
  const core = ringState(cluster, 0, networks.core.members);
  const management = ringState(cluster, 1, networks.management.members);

  return [
    {
      key: `${cluster.name}/core`,
      cluster: cluster.name,
      name: "Core",
      kind: "Core",
      subnet: networks.core.subnet,
      vip: null,
      detail: `MTU ${networks.core.mtu}`,
      tone: core.tone,
      status: core.status,
      members: networks.core.members.map((member) => member.node),
      of: cluster.nodes.length,
    },
    {
      key: `${cluster.name}/management`,
      cluster: cluster.name,
      name: "Management",
      kind: "Management",
      subnet: networks.management.subnet,
      // The floating address is its own row below, with its own state. Naming
      // it here too would say it twice and, worse, say it without a state.
      vip: null,
      detail: null,
      tone: management.tone,
      status: management.status,
      members: networks.management.members.map((member) => member.node),
      of: cluster.nodes.length,
    },
    // The cluster address gets a row of its own rather than a footnote on the
    // Management one. It is the address every console bookmark points at, it
    // has a state of its own that the ring's says nothing about, and burying
    // it in another row's cell is how an address nobody answers on goes
    // unnoticed.
    ...(cluster.vip
      ? [
          (() => {
            const state = vipState(cluster, cluster.vip);
            const holder = cluster.vip.state?.node;
            return {
              key: `${cluster.name}/vip`,
              cluster: cluster.name,
              name: "Cluster address",
              kind: "Management" as const,
              subnet: cluster.vip.address,
              vip: null,
              detail: holder ? `on ${holder}` : null,
              tone: state.tone,
              status: state.status,
              members: holder ? [holder] : [],
              of: cluster.nodes.length,
            };
          })(),
        ]
      : []),
    ...networks.external.map((network): NetworkRow => {
      const nodes = network.uplinks.map((uplink) => uplink.node);
      const state = externalState(cluster, nodes);
      return {
        key: `${cluster.name}/external/${network.name}`,
        cluster: cluster.name,
        name: network.name,
        kind: "External",
        subnet: null,
        vip: null,
        detail: `${network.bridge} · ${vlanText(network)}`,
        tone: state.tone,
        status: state.status,
        members: nodes,
        of: cluster.nodes.length,
      };
    }),
  ];
}

const columns: Column<NetworkRow>[] = [
  {
    key: "status",
    header: "Status",
    value: (row) => row.status,
    render: (row) => <Status tone={row.tone} label={row.status} />,
    sortable: true,
    width: 170,
  },
  {
    key: "cluster",
    header: "Cluster",
    value: (row) => row.cluster,
    render: (row) => (
      <span className="qz-mono text-[12px] truncate" title={row.cluster}>
        {row.cluster}
      </span>
    ),
    sortable: true,
    width: 160,
  },
  {
    key: "name",
    header: "Name",
    value: (row) => row.name,
    render: (row) => (
      <span className="text-[var(--qz-fg-1)] font-semibold qz-mono truncate">{row.name}</span>
    ),
    sortable: true,
    width: 160,
  },
  {
    key: "kind",
    header: "Type",
    value: (row) => row.kind,
    render: (row) => <span className="text-[var(--qz-fg-4)]">{row.kind}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "subnet",
    header: "Subnet",
    value: (row) => row.subnet ?? "",
    render: (row) =>
      row.subnet ? (
        <span className="whitespace-nowrap">
          {row.subnet}
          {row.vip && <span className="qz-dim"> · VIP {row.vip}</span>}
        </span>
      ) : (
        <Dash />
      ),
    mono: true,
    width: 220,
  },
  {
    key: "detail",
    header: "Detail",
    value: (row) => row.detail ?? "",
    render: (row) => row.detail || <Dash />,
    mono: true,
    width: 240,
  },
  {
    key: "members",
    header: "Members",
    value: (row) => String(row.members.length),
    render: (row) => (
      <span className="qz-mono whitespace-nowrap" title={row.members.join(", ")}>
        {row.members.length} / {row.of}
      </span>
    ),
    sortable: true,
    width: 110,
  },
];

function NetworksTable({
  rows,
  onRefresh,
  toolbar,
}: {
  rows: NetworkRow[];
  onRefresh: () => Promise<void>;
  toolbar?: React.ReactNode;
}) {
  const filters: FilterDef<NetworkRow>[] = useMemo(
    () => [
      {
        key: "cluster",
        label: "Cluster",
        options: Array.from(new Set(rows.map((row) => row.cluster)))
          .sort()
          .map((name) => ({ value: name, label: name })),
        predicate: (row, value) => row.cluster === value,
      },
      {
        key: "kind",
        label: "Type",
        options: Array.from(new Set(rows.map((row) => row.kind)))
          .sort()
          .map((kind) => ({ value: kind, label: kind })),
        predicate: (row, value) => row.kind === value,
      },
    ],
    [rows],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      toolbar={toolbar}
      rowId={(row) => row.key}
      storageKey="networking-networks"
      searchPlaceholder="Search networks…"
      emptyMessage="No cluster networks defined."
      onRefresh={onRefresh}
    />
  );
}

/// The three typed networks every cluster's members share — Core, Management,
/// External — read off the replicated record and joined with what corosync
/// reports about each ring. This page presents the shared definition; the
/// per-node links realizing it live on Networking → Interfaces.
export default function NetworksPage() {
  const [environment, setEnvironment] = useState<EnvironmentResponse | null>(null);
  const [answers, setAnswers] = useState<Record<string, NetworksAnswer>>({});
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const { setToast } = useConsole();

  const load = useCallback(async () => {
    try {
      // The uplink pickers need every member's links; a failure here costs
      // the pickers their options, not the page its table.
      void fetchInventory()
        .then(setInventory)
        .catch(() => setInventory(null));
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

  // One flat list across every cluster: the Cluster column is what tells them
  // apart, so a reader comparing two clusters' Core subnets no longer has to
  // scroll between two sections to do it.
  const rows = clusters.flatMap((cluster) => {
    const answer = answers[cluster.name];
    if (!answer || "error" in answer) return [];
    return networkRows(cluster, answer.networks);
  });

  const unreadable = clusters.flatMap((cluster): [string, string][] => {
    const answer = answers[cluster.name];
    return answer && "error" in answer ? [[cluster.name, answer.error]] : [];
  });

  // Creating an External network needs a cluster to define it in. With none,
  // the control has nothing to act on and says so rather than opening a
  // dialog whose first field would be empty.
  const createControl = (
    <Button kind="primary" disabled={clusters.length === 0} onClick={() => setCreating(true)}>
      <Plus size={15} /> Create Network
    </Button>
  );

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

          {clusters.length > 0 && (
            <NetworksTable rows={rows} onRefresh={load} toolbar={createControl} />
          )}

          {/* A cluster whose record could not be read contributes no rows, so
              the reason is said here rather than leaving a table that is
              quietly short. */}
          {unreadable.map(([name, reason]) => (
            <div key={name} className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                <span className="qz-mono">{name}</span>: {reason}
              </div>
            </div>
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

      {creating && (
        <CreateExternalNetworkDialog
          clusters={clusters}
          inventory={inventory}
          onClose={() => setCreating(false)}
          onCreated={(message) => {
            setCreating(false);
            setToast(message);
            void load();
          }}
        />
      )}
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


/// The VLAN semantics, spelled the way the definition means them.
const vlanText = (network: ExternalNetwork): string =>
  network.mode === "trunk"
    ? network.allowed.length > 0
      ? `Trunk — VLANs ${network.allowed.join(", ")}`
      : "Trunk"
    : `Access — VLAN ${network.vlan}`;


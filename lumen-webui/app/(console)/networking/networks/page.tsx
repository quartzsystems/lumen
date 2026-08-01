"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { AlertTriangle, Plus, RotateCcw } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { RowActions } from "@/components/console/RowActions";
import { Status } from "@/components/dashboard/DashboardBits";
import { Button } from "@/components/ui/Button";
import { ApiError } from "@/lib/authClient";
import {
  fetchClusterNetworks,
  fetchEnvironment,
  forgetExternalNetwork,
  recoverClusterVip,
  type ClusterNetworks,
  type ClusterView,
  type EnvironmentResponse,
  type ExternalNetwork,
  type RingLink,
  type VipView,
} from "@/lib/clusterClient";
import {
  externalState,
  ringsByNode,
  ringState,
  vipState,
  type StatusTone,
} from "@/lib/networkStatus";
import { fetchInventory, type InventoryResponse } from "@/lib/inventoryClient";
import { shortNodeName, shortNodeNames } from "@/lib/nodeNames";
import { useConsole } from "@/lib/ConsoleContext";
import { CreateExternalNetworkDialog } from "@/components/network/CreateExternalNetworkDialog";
import { EditClusterVipDialog } from "@/components/network/EditClusterVipDialog";
import { EditCoreNetworkDialog } from "@/components/network/EditCoreNetworkDialog";

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
  /// host is the whole point of that type. Only ever a subnet: the cluster
  /// address is a single address and belongs in `addresses`, where the thing
  /// above the column heading is true of what is under it.
  subnet: string | null;
  /// What answers on this network, per member — and for the cluster VIP
  /// row, the floating address itself.
  ///
  /// Its own column because the subnet alone does not answer the question an
  /// operator actually has here, which is "what do I type to reach that
  /// node?". Reading it off the Interfaces table means knowing which link
  /// carries which network first.
  addresses: { node: string; address: string }[];
  /// MTU for Core, VLAN semantics for External, nothing for Management.
  detail: string | null;
  tone: StatusTone;
  status: string;
  /// Which members carry it, for the column that answers "is this everywhere?"
  members: string[];
  of: number;
  /// What this row can be acted on as. The cluster VIP, the External
  /// networks, and Core each have their own edit; Management carries no
  /// actions — so the row says which it is rather than the actions column
  /// guessing from the name.
  acts: "core" | "vip" | "external" | null;
  /// The External network behind the row, for the edit dialog.
  external?: ExternalNetwork;
  /// The cluster VIP's state, for the recovery.
  vipState?: VipView;
}

/// The rows one cluster contributes: Core, Management, and each External
/// network it defines.
function networkRows(
  cluster: ClusterView,
  networks: ClusterNetworks,
  /// Every member's rings, gathered across the environment. Without it only
  /// the node serving the page has an observed link and the rest read as
  /// unknown — see `ringsByNode`.
  rings: Map<string, RingLink[]>,
): NetworkRow[] {
  const core = ringState(cluster, 0, networks.core.members, rings);
  const management = ringState(cluster, 1, networks.management.members, rings);

  const seats = (members: typeof networks.core.members) =>
    members.map((member) => ({ node: member.node, address: member.address }));

  return [
    {
      key: `${cluster.name}/core`,
      cluster: cluster.name,
      name: "Core",
      kind: "Core",
      subnet: networks.core.subnet,
      addresses: seats(networks.core.members),
      detail: `MTU ${networks.core.mtu}`,
      tone: core.tone,
      status: core.status,
      members: networks.core.members.map((member) => member.node),
      of: cluster.nodes.length,
      acts: "core",
    },
    {
      key: `${cluster.name}/management`,
      cluster: cluster.name,
      name: "Management",
      kind: "Management",
      subnet: networks.management.subnet,
      addresses: seats(networks.management.members),
      detail: null,
      tone: management.tone,
      status: management.status,
      members: networks.management.members.map((member) => member.node),
      of: cluster.nodes.length,
      acts: null,
    },
    // The cluster VIP gets a row of its own rather than a footnote on the
    // Management one. It is the address every console bookmark points at, it
    // has a state of its own that the ring's says nothing about, and burying
    // it in another row's cell is how an address nobody answers on goes
    // unnoticed.
    ...(cluster.vip
      ? [
          (() => {
            const vip = cluster.vip!;
            const state = vipState(cluster, vip);
            const holder = vip.state?.node;
            return {
              key: `${cluster.name}/vip`,
              cluster: cluster.name,
              name: "Cluster VIP",
              kind: "Management" as const,
              // It has no subnet of its own — it lives in Management's, whose
              // own row says so. Putting the address here was the column
              // heading claiming something it was not.
              subnet: null,
              addresses: [{ node: holder ?? "", address: vip.address }],
              detail: holder ? `on ${shortNodeName(holder)}` : null,
              tone: state.tone,
              status: state.status,
              members: holder ? [holder] : [],
              of: cluster.nodes.length,
              acts: "vip" as const,
              vipState: vip,
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
        // No host addressing at all — that is what makes it External rather
        // than a second Management network.
        subnet: null,
        addresses: [],
        detail: `${network.bridge} - ${vlanText(network)}`,
        tone: state.tone,
        status: state.status,
        members: nodes,
        of: cluster.nodes.length,
        acts: "external",
        external: network,
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
    // Whole, unlike a node's: a cluster's name carries no domain, and
    // trimming at a dot would eat part of the name an operator chose.
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
    render: (row) => (row.subnet ? <span className="whitespace-nowrap">{row.subnet}</span> : <Dash />),
    mono: true,
    width: 160,
  },
  {
    key: "addresses",
    header: "Addresses",
    // Sorted on the addresses themselves rather than the node names: an
    // operator scanning this column is looking for an address.
    value: (row) => row.addresses.map((seat) => seat.address).join(" "),
    render: (row) => <AddressesCell row={row} />,
    mono: true,
    width: 260,
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
      <span className="qz-mono whitespace-nowrap" title={shortNodeNames(row.members)}>
        {row.members.length} / {row.of}
      </span>
    ),
    sortable: true,
    width: 110,
  },
];

/// Who answers on this network, and where.
///
/// One address per member, each labelled with the member holding it — because
/// "which of these is the node I want" is the question, and a bare list of
/// four addresses does not answer it. The cluster VIP has no node to
/// label it with while it is stopped, and shows alone rather than beside an
/// empty name.
function AddressesCell({ row }: { row: NetworkRow }) {
  if (row.addresses.length === 0) return <Dash />;
  return (
    <span className="inline-flex flex-col gap-[2px] min-w-0">
      {row.addresses.map((seat) => (
        <span key={`${seat.node}/${seat.address}`} className="whitespace-nowrap truncate">
          {seat.node && (
            <span className="qz-dim" title={seat.node}>
              {shortNodeName(seat.node)}{" "}
            </span>
          )}
          {seat.address}
        </span>
      ))}
    </span>
  );
}

function NetworksTable({
  rows,
  onRefresh,
  toolbar,
  busy,
  onEdit,
  onDelete,
  onRecover,
}: {
  rows: NetworkRow[];
  onRefresh: () => Promise<void>;
  toolbar?: React.ReactNode;
  busy: boolean;
  onEdit: (row: NetworkRow) => void;
  onDelete: (row: NetworkRow) => Promise<void>;
  onRecover: (row: NetworkRow) => Promise<void>;
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
      actions={(row) => (
        <div className="flex items-center gap-1">
          {/* Offered only on the address that is actually failing. A
              recovery on a running one would clear a history nobody is
              reading and re-probe a resource that is already up. */}
          {row.acts === "vip" && row.tone === "crit" && (
            <button
              type="button"
              className="icon-button"
              disabled={busy}
              title="Clear the recorded failure and probe the address again — do this after fixing what stopped it"
              onClick={() => void onRecover(row)}
            >
              <RotateCcw size={15} />
            </button>
          )}
          <RowActions
            label={row.name}
            onEdit={() => onEdit(row)}
            onDelete={() => onDelete(row)}
            editDisabled={busy || row.acts === null}
            editTitle={
              row.acts === null
                ? "Management adopts the addressing the nodes already have — it is defined when the cluster is created"
                : undefined
            }
            // The cluster VIP is removed by editing it — clearing the
            // field is what "no cluster VIP" means, and a delete control
            // beside it would be a second way to say the same thing.
            deleteDisabled={busy || row.acts !== "external"}
            deleteTitle={
              row.acts === "vip"
                ? "Clear the address in the edit dialog to remove it"
                : row.acts === "core" || row.acts === null
                  ? "Core and Management go when the cluster does"
                  : undefined
            }
          />
        </div>
      )}
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
  /// The External network being changed, with the cluster it belongs to —
  /// the table spans clusters, so a name alone does not identify one.
  const [editing, setEditing] = useState<{ cluster: string; network: ExternalNetwork } | null>(
    null,
  );
  /// The cluster whose address is being changed.
  const [editingVip, setEditingVip] = useState<string | null>(null);
  /// The cluster whose Core network is being changed.
  const [editingCore, setEditingCore] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
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
    return networkRows(cluster, answer.networks, ringsByNode(cluster, inventory));
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

  /// One action, its outcome reported the same way whichever it was. The
  /// reload afterwards is what makes the table show what the cluster now
  /// says rather than what the console asked for.
  ///
  /// An empty `success` leaves the toast to the action: some outcomes are not
  /// a single sentence known in advance.
  const run = async (action: () => Promise<unknown>, success: string) => {
    setBusy(true);
    try {
      await action();
      if (success) setToast(success);
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Something went wrong.");
    } finally {
      setBusy(false);
    }
  };

  const edit = (row: NetworkRow) => {
    if (row.acts === "vip") setEditingVip(row.cluster);
    else if (row.acts === "core") setEditingCore(row.cluster);
    else if (row.external) setEditing({ cluster: row.cluster, network: row.external });
  };

  const remove = (row: NetworkRow) =>
    run(
      () => forgetExternalNetwork(row.cluster, row.name),
      `${row.name} is no longer defined. Its bridges were left on each member — remove them on Interfaces if nothing is using them.`,
    );

  // The recovery answers with the address as Pacemaker has it a moment
  // later, and that answer is the point: a cause that is still there comes
  // back saying so, and telling the operator "recovered" would be a lie the
  // next poll would contradict anyway.
  const recover = (row: NetworkRow) =>
    run(async () => {
      const vip = await recoverClusterVip(row.cluster);
      const state = vip.state;
      setToast(
        state?.active
          ? `The cluster VIP is up on ${shortNodeName(state.node ?? "a member")}.`
          : `The failure was cleared and the address probed again — Pacemaker still reports ${
              state?.reason ?? state?.role ?? "it stopped"
            }. Whatever stopped it is still there.`,
      );
    }, "");

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
            <NetworksTable
              rows={rows}
              onRefresh={load}
              toolbar={createControl}
              busy={busy}
              onEdit={edit}
              onDelete={remove}
              onRecover={recover}
            />
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

      {/* The same dialog, opened against an existing definition. One form
          rather than two: the fields an External network has do not change
          because it already exists, and two forms is two places for the
          every-member rule to be spelled differently. */}
      {editing && (
        <CreateExternalNetworkDialog
          clusters={clusters}
          inventory={inventory}
          editing={editing}
          onClose={() => setEditing(null)}
          onCreated={(message) => {
            setEditing(null);
            setToast(message);
            void load();
          }}
        />
      )}

      {editingCore &&
        (() => {
          const answer = answers[editingCore];
          const networks = answer && !("error" in answer) ? answer.networks : null;
          return networks ? (
            <EditCoreNetworkDialog
              cluster={editingCore}
              networks={networks}
              inventory={inventory}
              onClose={() => setEditingCore(null)}
              onSaved={(message) => {
                setEditingCore(null);
                setToast(message);
                void load();
              }}
            />
          ) : null;
        })()}

      {editingVip && (
        <EditClusterVipDialog
          cluster={clusters.find((c) => c.name === editingVip) ?? null}
          networks={
            (() => {
              const answer = answers[editingVip];
              return answer && !("error" in answer) ? answer.networks : null;
            })()
          }
          onClose={() => setEditingVip(null)}
          onSaved={(message) => {
            setEditingVip(null);
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


"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Info, Plus, Trash2 } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { Meter } from "@/components/vm/VmBits";
import { CreatePoolDialog, DestroyPoolDialog } from "@/components/storage/CreatePoolDialog";
import { PoolAcrossNodesDialog } from "@/components/storage/PoolAcrossNodesDialog";
import { ReplicatedVolumesSection } from "@/components/storage/ReplicatedVolumes";
import {
  fetchInventory,
  pooledStorage,
  unreachable,
  type InventoryResponse,
} from "@/lib/inventoryClient";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { titleCase, titleCaseOptions } from "@/lib/labels";
import {
  destroyPool,
  fetchPools,
  HEALTH_TONE,
  type PoolsResponse,
  type PoolView,
} from "@/lib/storageClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 10000;

/// The node's storage pools.
///
/// Creating one destroys whatever was on the disks it is given, which is why
/// the dialog behind Create reports what is already on each disk and refuses
/// one that is spoken for without the acknowledgement. The pool this appliance
/// is installed on is never destroyable — the backend says so, and the control
/// carries its reason rather than being silently grey.
export default function StoragePage() {
  const { setToast } = useConsole();
  const [pools, setPools] = useState<PoolsResponse | null>(null);
  /// What every member has, for the cluster capacity figures and the
  /// cross-node drive picker. Null on a node whose peers cannot be asked,
  /// which is also the standalone case.
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [poolingAcrossNodes, setPoolingAcrossNodes] = useState(false);
  const [destroying, setDestroying] = useState<PoolView | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      // The environment read reaches other nodes; a member being away must
      // not cost this page the local pool table it exists to show.
      void fetchInventory()
        .then(setInventory)
        .catch(() => setInventory(null));
      setPools(await fetchPools());
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the storage on this node.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    // Polling pauses while a dialog is open, so a refresh cannot move the
    // picker out from under the operator mid-choice.
    if (creating || destroying) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load, creating, destroying]);

  const existingNames = useMemo(
    () => (pools?.nodes ?? []).flatMap((node) => node.pools.map((pool) => pool.name)),
    [pools],
  );

  const destroy = async (pool: PoolView, acknowledge: boolean) => {
    setBusy(true);
    try {
      await destroyPool(pool.name, acknowledge);
      setDestroying(null);
      setToast(`${pool.name} destroyed.`);
      await load();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not destroy the pool.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Page>
      <PageHeader title="Storage" description="Pools on this node, and how much of each is in use." />

      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          <div className="callout">
            <Info size={17} className="flex-shrink-0 text-[var(--qz-fg-4)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-3)]">
              Creating and destroying a pool are the two storage operations that cannot happen
              inside this daemon&apos;s sandbox — they write{" "}
              <span className="qz-mono">/etc/zfs/zpool.cache</span> — so they are handed to systemd
              and run outside it. Nothing was loosened to make them work. Virtual machine disks are
              created under each pool&apos;s <span className="qz-mono">lumen</span> dataset, from
              the machine that needs them.
            </div>
          </div>

          {pools === null && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the storage…</div>
          )}

          {/* Only once there is more than one member. On a standalone
              appliance the pool table below is the whole truth, and a
              "cluster total" of one node's pools is noise. */}
          {(inventory?.members.length ?? 0) > 1 && (
            <ClusterCapacity
              inventory={inventory}
              onPoolAcrossNodes={() => setPoolingAcrossNodes(true)}
            />
          )}

          {pools?.nodes.map((node) => (
            <section key={node.node} className="flex flex-col gap-2">
              {/* One appliance is the usual case, and naming it then is noise. */}
              {(pools?.nodes.length ?? 0) > 1 && (
                <h2
                  className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0"
                  style={{ fontFamily: "var(--qz-font-mono)" }}
                >
                  {node.node}
                </h2>
              )}
              <PoolTable
                rows={node.pools}
                busy={busy}
                onRefresh={load}
                onCreate={() => setCreating(true)}
                onDestroy={setDestroying}
              />
            </section>
          ))}

          {/* Cluster-scoped storage: present only when this node is in an
              environment with clusters, and absent — not empty — otherwise. */}
          <ReplicatedVolumesSection />
        </div>
      </PageBody>

      {creating && (
        <CreatePoolDialog
          existingNames={existingNames}
          onClose={() => setCreating(false)}
          onCreated={async (pool) => {
            setCreating(false);
            setToast(`${pool.name} created — ${formatBytes(pool.size)}.`);
            await load();
          }}
        />
      )}

      {poolingAcrossNodes && (
        <PoolAcrossNodesDialog
          inventory={inventory}
          onClose={() => setPoolingAcrossNodes(false)}
          onCreated={async (message) => {
            setPoolingAcrossNodes(false);
            setToast(message);
            await load();
          }}
        />
      )}

      {destroying && (
        <DestroyPoolDialog
          pool={destroying}
          busy={busy}
          onClose={() => setDestroying(null)}
          onConfirm={(acknowledge) => destroy(destroying, acknowledge)}
        />
      )}
    </Page>
  );
}

const columns: Column<PoolView>[] = [
  {
    key: "name",
    header: "Name",
    value: (pool) => pool.name,
    sortable: true,
    width: 160,
    render: (pool) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {pool.name}
      </span>
    ),
  },
  {
    key: "health",
    header: "Health",
    value: (pool) => pool.health,
    sortable: true,
    width: 120,
    render: (pool) => (
      <span className={`badge badge-${HEALTH_TONE[pool.health]}`}>{titleCase(pool.health)}</span>
    ),
  },
  {
    key: "size",
    header: "Size",
    value: (pool) => pool.size,
    render: (pool) => <span className="qz-mono">{formatBytes(pool.size)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "allocated",
    header: "Allocated",
    value: (pool) => pool.allocated,
    render: (pool) => <span className="qz-mono">{formatBytes(pool.allocated)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "free",
    header: "Free",
    value: (pool) => pool.free,
    render: (pool) => <span className="qz-mono">{formatBytes(pool.free)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "usage",
    header: "Used",
    value: (pool) => pool.used_percent,
    sortable: true,
    width: 170,
    render: (pool) => (
      <span className="inline-flex items-center gap-[10px] w-full min-w-0">
        <Meter
          percent={pool.used_percent}
          title={`${formatBytes(pool.allocated)} of ${formatBytes(pool.size)}`}
        />
        <span className="qz-mono text-[12px] text-[var(--qz-fg-3)] flex-shrink-0">
          {pool.used_percent}%
        </span>
      </span>
    ),
  },
  {
    key: "fragmentation",
    header: "Fragmentation",
    value: (pool) => pool.fragmentation ?? "",
    render: (pool) =>
      pool.fragmentation === null ? (
        <Dash />
      ) : (
        <span className="qz-mono">{pool.fragmentation}%</span>
      ),
    sortable: true,
    width: 130,
  },
  {
    key: "read_only",
    header: "Writable",
    value: (pool) => (pool.read_only ? "no" : "yes"),
    render: (pool) =>
      pool.read_only ? (
        <span className="badge badge-warn">Read only</span>
      ) : (
        <span className="qz-dim">Yes</span>
      ),
    width: 110,
  },
];

/// What the members' pools add up to, and the way in to building more.
///
/// Every figure here is raw capacity, said so in the panel rather than only
/// in a tooltip. A replicated volume consumes its full size on every member
/// holding a replica, so the sum of the pools is what the hardware is — not
/// what a machine may be given. An operator who reads it as usable will size
/// their volumes wrong by exactly the replica count.
function ClusterCapacity({
  inventory,
  onPoolAcrossNodes,
}: {
  inventory: InventoryResponse | null;
  onPoolAcrossNodes: () => void;
}) {
  const total = pooledStorage(inventory);
  const missing = unreachable(inventory);
  const used = total.size > 0 ? (total.allocated / total.size) * 100 : 0;

  return (
    <section className="surface p-5 flex flex-col gap-4">
      <header className="flex items-start gap-4">
        <div className="flex-1 min-w-0">
          <h2 className="text-[14px] font-semibold text-[var(--qz-fg-1)] m-0">
            Across the environment
          </h2>
          <p className="text-[12px] text-[var(--qz-fg-4)] mt-1 mb-0">
            {total.pools} {total.pools === 1 ? "pool" : "pools"} on {total.counted} of {total.of}{" "}
            members. Raw capacity — a replicated volume costs its full size on every member
            holding a replica.
          </p>
        </div>
        <Button kind="primary" size="sm" icon={Plus} onClick={onPoolAcrossNodes}>
          Pool drives across nodes
        </Button>
      </header>

      {missing.length > 0 && (
        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {missing.map((member) => member.node).join(", ")} could not be asked, so nothing they
            hold is counted here.
          </div>
        </div>
      )}

      <div className="grid gap-4" style={{ gridTemplateColumns: "repeat(3, 1fr)" }}>
        <Figure label="Raw capacity" value={formatBytes(total.size)} />
        <Figure label="Allocated" value={formatBytes(total.allocated)} />
        <Figure label="Free" value={formatBytes(total.free)} />
      </div>
      <Meter percent={used} />
    </section>
  );
}

function Figure({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col gap-1 min-w-0">
      <span className="text-[11px] uppercase tracking-wide text-[var(--qz-fg-4)]">{label}</span>
      <span className="qz-mono text-[15px] text-[var(--qz-fg-1)]">{value}</span>
    </div>
  );
}

function PoolTable({
  rows,
  busy,
  onRefresh,
  onCreate,
  onDestroy,
}: {
  rows: PoolView[];
  busy: boolean;
  onRefresh: () => Promise<void>;
  onCreate: () => void;
  onDestroy: (pool: PoolView) => void;
}) {
  const filters: FilterDef<PoolView>[] = useMemo(
    () => [
      {
        key: "health",
        label: "Health",
        // The option value stays the wire one the predicate matches on; only
        // the label an operator reads is capitalised.
        options: titleCaseOptions(Array.from(new Set(rows.map((pool) => pool.health))).sort()),
        predicate: (pool, value) => pool.health === value,
      },
    ],
    [rows],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      rowId={(pool) => pool.name}
      storageKey="storage-pools"
      searchPlaceholder="Search pools…"
      emptyMessage="No storage pools on this node."
      onRefresh={onRefresh}
      toolbar={
        <Button kind="primary" size="sm" icon={Plus} onClick={onCreate}>
          Create
        </Button>
      }
      actionsWidth={90}
      actions={(pool) => (
        // A disabled control explains itself: the backend supplies the reason,
        // so the console and the node can never disagree about what is
        // possible.
        <span title={pool.destroy_blocked_reason ?? undefined}>
          <Button
            kind="ghost"
            size="sm"
            icon={Trash2}
            disabled={busy || !pool.destroyable}
            onClick={() => onDestroy(pool)}
          />
        </span>
      )}
    />
  );
}

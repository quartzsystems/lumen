"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Info, Plus, Trash2 } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { Meter } from "@/components/vm/VmBits";
import { CreatePoolDialog, DestroyPoolDialog } from "@/components/storage/CreatePoolDialog";
import {
  fetchInventory,
  poolsByMember,
  pooledStorage,
  unreachable,
  type InventoryResponse,
  type OwnedPool,
} from "@/lib/inventoryClient";
import { ApiError } from "@/lib/authClient";
import { useConsole } from "@/lib/ConsoleContext";
import { titleCase, titleCaseOptions } from "@/lib/labels";
import { shortNodeName } from "@/lib/nodeNames";
import { destroyPool, HEALTH_TONE, type PoolView } from "@/lib/storageClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 10000;

/// Every member's storage pools, in one table.
///
/// One table rather than one section per node, for the same reason Interfaces
/// is: the question an operator asks here — "which node still has room?",
/// "does every member have a pool called tank?" — is asked across the
/// environment, and answering it by scrolling between sections is the console
/// making them do the join by hand. The node becomes a column.
///
/// Creating a pool destroys whatever was on the disks it is given, which is
/// why the dialog behind Create reports what is already on each disk and
/// refuses one that is spoken for without the acknowledgement. Destroying
/// stays with the node that owns the pool.
export default function PoolsPage() {
  const { setToast } = useConsole();
  /// What every member has. A standalone appliance answers with itself, so
  /// this is the one read the page needs either way.
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState<PoolView | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      setInventory(await fetchInventory());
      setError(null);
    } catch (err) {
      // A 401 has already redirected to /login.
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the storage.");
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

  const rows = useMemo(() => poolsByMember(inventory), [inventory]);
  // Every pool name in the environment. A create is refused for a name this
  // node already has; naming the others is still worth it, because a name
  // taken elsewhere is one a replicated volume cannot use.
  const existingNames = useMemo(() => rows.map((row) => row.pool.name), [rows]);
  const missing = unreachable(inventory);

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
      <PageHeader
        title="Pools"
        description="Storage pools across the environment, and how much of each is in use."
      />

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

          {inventory === null && !error && (
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the storage…</div>
          )}

          {/* A member that could not be asked contributes no rows, so the
              reason is said here rather than leaving a table that is quietly
              short. */}
          {missing.length > 0 && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {missing.map((member) => shortNodeName(member.node)).join(", ")} could not be asked,
                so nothing {missing.length === 1 ? "it holds is" : "they hold is"} listed or counted
                here.
                {missing[0]?.error && (
                  <span className="text-[var(--qz-fg-4)]"> {missing[0].error}</span>
                )}
              </div>
            </div>
          )}

          {/* Only once there is more than one member. On a standalone
              appliance the table below is the whole truth, and a "cluster
              total" of one node's pools is noise. */}
          {(inventory?.members.length ?? 0) > 1 && <ClusterCapacity inventory={inventory} />}

          {inventory !== null && (
            <PoolTable
              rows={rows}
              busy={busy}
              onRefresh={load}
              onCreate={() => setCreating(true)}
              onDestroy={setDestroying}
            />
          )}
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

const columns: Column<OwnedPool>[] = [
  {
    key: "node",
    header: "Node",
    value: (row) => row.node,
    sortable: true,
    width: 150,
    // The domain is identical on every row; the whole name is one hover away.
    render: (row) => (
      <span className="qz-mono text-[12px] truncate" title={row.node}>
        {shortNodeName(row.node)}
      </span>
    ),
  },
  {
    key: "name",
    header: "Name",
    value: (row) => row.pool.name,
    sortable: true,
    width: 160,
    render: (row) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {row.pool.name}
      </span>
    ),
  },
  {
    key: "health",
    header: "Health",
    value: (row) => row.pool.health,
    sortable: true,
    width: 120,
    render: (row) => (
      <span className={`badge badge-${HEALTH_TONE[row.pool.health]}`}>
        {titleCase(row.pool.health)}
      </span>
    ),
  },
  {
    key: "size",
    header: "Size",
    value: (row) => row.pool.size,
    render: (row) => <span className="qz-mono">{formatBytes(row.pool.size)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "allocated",
    header: "Allocated",
    value: (row) => row.pool.allocated,
    render: (row) => <span className="qz-mono">{formatBytes(row.pool.allocated)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "free",
    header: "Free",
    value: (row) => row.pool.free,
    render: (row) => <span className="qz-mono">{formatBytes(row.pool.free)}</span>,
    sortable: true,
    width: 120,
  },
  {
    key: "usage",
    header: "Used",
    value: (row) => row.pool.used_percent,
    sortable: true,
    width: 170,
    render: (row) => (
      <span className="inline-flex items-center gap-[10px] w-full min-w-0">
        <Meter
          percent={row.pool.used_percent}
          title={`${formatBytes(row.pool.allocated)} of ${formatBytes(row.pool.size)}`}
        />
        <span className="qz-mono text-[12px] text-[var(--qz-fg-3)] flex-shrink-0">
          {row.pool.used_percent}%
        </span>
      </span>
    ),
  },
  {
    key: "fragmentation",
    header: "Fragmentation",
    value: (row) => row.pool.fragmentation ?? "",
    render: (row) =>
      row.pool.fragmentation === null || row.pool.fragmentation === undefined ? (
        <Dash />
      ) : (
        <span className="qz-mono">{row.pool.fragmentation}%</span>
      ),
    sortable: true,
    width: 130,
  },
  {
    key: "read_only",
    header: "Writable",
    value: (row) => (row.pool.read_only ? "no" : "yes"),
    render: (row) =>
      row.pool.read_only ? (
        <span className="badge badge-warn">Read only</span>
      ) : (
        <span className="qz-dim">Yes</span>
      ),
    width: 110,
  },
];

/// What the members' pools add up to.
///
/// Every figure here is raw capacity, said so in the panel rather than only
/// in a tooltip. A pooled disk consumes its full size on every member, so
/// the sum of the pools is what the hardware is — not what a machine may be
/// given.
function ClusterCapacity({ inventory }: { inventory: InventoryResponse | null }) {
  const total = pooledStorage(inventory);
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
            members. Raw capacity — a pooled disk costs its full size on every member.
          </p>
        </div>
      </header>

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
  rows: OwnedPool[];
  busy: boolean;
  onRefresh: () => Promise<void>;
  onCreate: () => void;
  onDestroy: (pool: PoolView) => void;
}) {
  const filters: FilterDef<OwnedPool>[] = useMemo(
    () => [
      {
        key: "node",
        label: "Node",
        // The value stays the whole name — it is what every request carries —
        // and only the label is shortened.
        options: Array.from(new Set(rows.map((row) => row.node)))
          .sort()
          .map((node) => ({ value: node, label: shortNodeName(node) })),
        predicate: (row, value) => row.node === value,
      },
      {
        key: "health",
        label: "Health",
        // The option value stays the wire one the predicate matches on; only
        // the label an operator reads is capitalised.
        options: titleCaseOptions(Array.from(new Set(rows.map((row) => row.pool.health))).sort()),
        predicate: (row, value) => row.pool.health === value,
      },
    ],
    [rows],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={filters}
      // Two members may each have a pool called tank; the node is what makes
      // a row identity unique across the environment.
      rowId={(row) => `${row.node}/${row.pool.name}`}
      storageKey="storage-pools"
      searchPlaceholder="Search pools…"
      emptyMessage="No storage pools yet."
      onRefresh={onRefresh}
      toolbar={
        <Button kind="primary" size="sm" icon={Plus} onClick={onCreate}>
          Create
        </Button>
      }
      actionsWidth={90}
      actions={(row) => (
        // A disabled control explains itself: the owning node supplies the
        // reason, so the console and the node can never disagree about what is
        // possible. A pool on another member is named by its owner — the
        // destroy lands on the node that has it.
        <span
          title={
            row.local
              ? (row.pool.destroy_blocked_reason ?? undefined)
              : `Owned by ${shortNodeName(row.node)} — destroy it on ${shortNodeName(row.node)}`
          }
        >
          <Button
            kind="ghost"
            size="sm"
            icon={Trash2}
            disabled={busy || !row.local || !row.pool.destroyable}
            onClick={() => onDestroy(row.pool)}
          />
        </span>
      )}
    />
  );
}

"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Info, Plus, Trash2 } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { Meter } from "@/components/vm/VmBits";
import { CreatePoolDialog, DestroyPoolDialog } from "@/components/storage/CreatePoolDialog";
import { ReplicatedVolumesSection } from "@/components/storage/ReplicatedVolumes";
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
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [destroying, setDestroying] = useState<PoolView | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
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

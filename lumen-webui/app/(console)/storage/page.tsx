"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Info } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Meter } from "@/components/vm/VmBits";
import { ApiError } from "@/lib/authClient";
import { titleCase, titleCaseOptions } from "@/lib/labels";
import { fetchPools, HEALTH_TONE, type PoolsResponse, type PoolView } from "@/lib/storageClient";
import { formatBytes } from "@/lib/vmClient";

const POLL_MS = 10000;

/// The node's storage pools.
///
/// Read-only at this stage, and deliberately so: creating a pool is the one
/// operation with no privileged daemon to delegate to, and doing it from a
/// hardened service would mean weakening the service. There is no Create
/// button because a button that cannot work is worse than no button — see
/// docs/compute.md.
export default function StoragePage() {
  const [pools, setPools] = useState<PoolsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

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
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

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
              Pools are created, imported, and removed from the node itself for now. Those are the
              operations that need privileges the management daemon deliberately does not have, so
              they will arrive with the small privileged helper that gets them — not by loosening
              this one. Virtual machine disks are already created from here, under each
              pool&apos;s <span className="qz-mono">lumen</span> dataset.
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
              <PoolTable rows={node.pools} onRefresh={load} />
            </section>
          ))}
        </div>
      </PageBody>
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

function PoolTable({ rows, onRefresh }: { rows: PoolView[]; onRefresh: () => Promise<void> }) {
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
    />
  );
}

"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Info } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { PooledVolumesSection } from "@/components/storage/PooledStorage";
import { ApiError } from "@/lib/authClient";
import { shortNodeName } from "@/lib/nodeNames";
import { fetchPools, fetchVolumes, type VolumeView } from "@/lib/storageClient";
import { formatBytes } from "@/lib/vmClient";

/// The volumes themselves: machine disks and whatever else has been carved out
/// of a pool.
///
/// This page used to be the cluster pool's page, which put a pool on the one
/// page named after the things pools contain. The pool moved next door, under
/// Pools, with the nodes' own pools — and what is left here is the object an
/// operator comes looking for: a disk, by name, wherever it happens to live.
///
/// Two sources, because there are two ways a machine disk is stored on this
/// appliance and neither is more real than the other. A pooled disk is served
/// as the same device on every member; a ZFS volume belongs to the node whose
/// pool it was carved from. They are listed separately rather than merged,
/// because the questions worth asking about them differ — which member is
/// writing to it, against how much of its reservation is referenced — and a
/// merged table would answer neither well.
export default function VolumesPage() {
  const [rows, setRows] = useState<ZfsVolume[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const pools = await fetchPools();
      const listings = await Promise.all(
        pools.nodes.flatMap((node) =>
          node.pools.map(async (pool) => {
            const answer = await fetchVolumes(pool.name);
            return answer.volumes
              // Volumes only. A pool's datasets are its own furniture — the
              // `lumen` dataset the disks are carved from, a media library —
              // and this page is about the things a machine is given.
              .filter((volume) => volume.kind === "volume")
              .map((volume) => ({ node: answer.node, pool: pool.name, volume }));
          }),
        ),
      );
      setRows(listings.flat());
      setError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not read the volumes.");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <Page>
      <PageHeader
        title="Volumes"
        description="Machine disks and the other volumes carved out of this appliance's storage."
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
              A volume is created for the machine that needs it, from that machine&apos;s own page —
              there is no create here, because a disk with no machine is the thing this page exists
              to help you find and clear up. The pools they come out of are under{" "}
              <strong>Storage → Pools</strong>.
            </div>
          </div>

          <PooledVolumesSection />

          <section className="flex flex-col gap-2">
            <div className="flex items-center gap-3">
              <h2 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">ZFS volumes</h2>
              <span
                className="text-[12px] text-[var(--qz-fg-3)]"
                title="Read from this node's own pools. A volume on another node is listed on that node's console."
              >
                on this node&apos;s pools
              </span>
            </div>
            <ZfsVolumesTable rows={rows ?? []} onRefresh={load} />
          </section>
        </div>
      </PageBody>
    </Page>
  );
}

/// One ZFS volume, with the pool and node it belongs to folded in — the two
/// facts the dataset name only half carries.
interface ZfsVolume {
  node: string;
  pool: string;
  volume: VolumeView;
}

/// The dataset name without the pool it is already filed under.
const shortVolumeName = (row: ZfsVolume): string =>
  row.volume.name.startsWith(`${row.pool}/`)
    ? row.volume.name.slice(row.pool.length + 1)
    : row.volume.name;

function ZfsVolumesTable({ rows, onRefresh }: { rows: ZfsVolume[]; onRefresh: () => void }) {
  const columns: Column<ZfsVolume>[] = [
    {
      key: "name",
      header: "Volume",
      value: (row) => shortVolumeName(row),
      sortable: true,
      width: 240,
      render: (row) => (
        <span
          className="text-[var(--qz-fg-1)] font-semibold truncate"
          style={{ fontFamily: "var(--qz-font-mono)" }}
          title={row.volume.name}
        >
          {shortVolumeName(row)}
        </span>
      ),
    },
    {
      key: "pool",
      header: "Pool",
      value: (row) => row.pool,
      sortable: true,
      mono: true,
      width: 140,
    },
    {
      key: "node",
      header: "Node",
      value: (row) => row.node,
      sortable: true,
      width: 150,
      render: (row) => (
        <span className="qz-mono text-[12px] truncate" title={row.node}>
          {shortNodeName(row.node)}
        </span>
      ),
    },
    {
      key: "volsize",
      header: "Size",
      value: (row) => row.volume.volsize ?? 0,
      sortable: true,
      width: 110,
      render: (row) =>
        row.volume.volsize === null ? (
          <Dash />
        ) : (
          <span className="qz-mono">{formatBytes(row.volume.volsize)}</span>
        ),
    },
    {
      key: "used",
      header: "Used",
      value: (row) => row.volume.used,
      sortable: true,
      width: 110,
      render: (row) => <span className="qz-mono">{formatBytes(row.volume.used)}</span>,
    },
    {
      key: "referenced",
      header: "Referenced",
      value: (row) => row.volume.referenced,
      sortable: true,
      width: 120,
      render: (row) => <span className="qz-mono">{formatBytes(row.volume.referenced)}</span>,
    },
    {
      key: "volblocksize",
      header: "Block size",
      value: (row) => row.volume.volblocksize ?? 0,
      width: 110,
      render: (row) =>
        row.volume.volblocksize === null ? (
          <Dash />
        ) : (
          <span className="qz-mono">{formatBytes(row.volume.volblocksize)}</span>
        ),
    },
    {
      key: "managed",
      header: "Created by",
      value: (row) => (row.volume.lumen_managed ? "Lumen" : "elsewhere"),
      width: 130,
      render: (row) =>
        row.volume.lumen_managed ? (
          <span className="badge badge-info">Lumen</span>
        ) : (
          // Not Lumen's, and therefore not Lumen's to remove — worth saying
          // plainly rather than leaving an operator to wonder why nothing here
          // offers to delete it.
          <span className="qz-dim" title="Made outside Lumen, so Lumen will not remove it.">
            elsewhere
          </span>
        ),
    },
  ];

  const filters: FilterDef<ZfsVolume>[] = useMemo(
    () => [
      {
        key: "pool",
        label: "Pool",
        options: Array.from(new Set(rows.map((row) => row.pool)))
          .sort()
          .map((pool) => ({ value: pool, label: pool })),
        predicate: (row, value) => row.pool === value,
      },
    ],
    [rows],
  );

  return (
    <DataTable
      rows={rows}
      columns={columns}
      filters={rows.length > 0 ? filters : []}
      rowId={(row) => `${row.node}/${row.volume.name}`}
      storageKey="storage-volumes"
      searchPlaceholder="Search volumes…"
      emptyMessage="No volumes on this node's pools."
      onRefresh={onRefresh}
    />
  );
}

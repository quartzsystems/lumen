"use client";

import { Panel } from "@/components/vm/VmBits";
import { formatBytes, formatMib } from "@/lib/vmClient";
import { nodeTone, type ClusterNodeView } from "@/lib/clusterClient";
import type { NodeInventory } from "@/lib/inventoryClient";

/// One node's own page: what it is, what it is made of, and what it is
/// carrying — the questions an operator opens a node to ask, answered from
/// what the environment and the inventory already say rather than from a
/// new endpoint.
///
/// Every figure is the node's own report. A node that could not be asked
/// shows the fact rather than zeros: a blank capacity is a hypervisor that
/// did not answer, and reading it as "no processors" would be a lie the
/// page tells about a machine that is probably fine.
export function NodeOverview({
  node,
  cluster,
  inventory,
}: {
  node: ClusterNodeView;
  cluster: string | null;
  inventory: NodeInventory | null;
}) {
  const tone = nodeTone(node);
  const capacity = inventory?.capacity ?? null;
  const pools = inventory?.pools ?? [];
  const rawStorage = pools.reduce((sum, pool) => sum + pool.size, 0);
  const usedStorage = pools.reduce((sum, pool) => sum + pool.allocated, 0);
  const links = inventory?.interfaces ?? [];

  return (
    <div className="flex flex-col gap-4">
      <Panel title="Status">
        <div className="flex flex-col gap-3">
          <div className="flex items-center gap-3 flex-wrap">
            <span className={`badge badge-${tone.tone}`}>{tone.label}</span>
            {node.maintenance && <span className="badge badge-warn">Maintenance</span>}
            {node.local && <span className="badge badge-info">This node</span>}
            {node.unclean && (
              <span className="badge badge-crit" title="Lost and not yet fenced.">
                Unclean
              </span>
            )}
          </div>
          <Facts
            rows={[
              ["Cluster", cluster ?? "Standalone — not a member of any cluster"],
              ["Address", node.address ?? "—"],
              ["Control plane", node.controlplane_version ?? "—"],
              [
                "Fencing",
                node.fence
                  ? `${node.fence.active ? "Healthy" : "Stopped"}${
                      node.fence.bmc_address ? ` — ${node.fence.bmc_address}` : ""
                    }${node.fence.reason ? ` (${node.fence.reason})` : ""}`
                  : "No fence device",
              ],
            ]}
          />
        </div>
      </Panel>

      <Panel title="Capacity">
        {capacity ? (
          <div className="flex flex-col gap-3">
            <Meter
              label="Processors"
              detail={`${capacity.used_vcpus} of ${capacity.cpus} allocated to machines`}
              used={capacity.used_vcpus}
              total={capacity.cpus}
            />
            <Meter
              label="Memory"
              detail={`${formatMib(capacity.used_memory_mib)} of ${formatMib(
                capacity.memory_mib,
              )} allocated`}
              used={capacity.used_memory_mib}
              total={capacity.memory_mib}
            />
            {rawStorage > 0 && (
              <Meter
                label="Local storage"
                detail={`${formatBytes(usedStorage)} of ${formatBytes(rawStorage)} allocated across ${
                  pools.length
                } pool${pools.length === 1 ? "" : "s"}`}
                used={usedStorage}
                total={rawStorage}
              />
            )}
          </div>
        ) : (
          <div className="text-[13px] text-[var(--qz-fg-3)]">
            This node&apos;s hypervisor did not answer, so its capacity is unknown — which is not
            the same as empty.
          </div>
        )}
      </Panel>

      <Panel title="Networking">
        {node.rings.length > 0 && (
          <div className="flex flex-col gap-1 mb-3">
            {node.rings.map((ring) => (
              <div key={ring.link} className="text-[13px] text-[var(--qz-fg-2)]">
                <span className="qz-mono">Ring {ring.link}</span>{" "}
                <span className={ring.connected ? "" : "text-[var(--qz-danger)]"}>
                  {ring.connected ? "connected" : "faulty"}
                </span>
                {ring.address && <span className="qz-dim"> — {ring.address}</span>}
              </div>
            ))}
          </div>
        )}
        {links.length > 0 ? (
          <Facts
            rows={links
              // The links that carry something: a node has a dozen NICs and
              // the ones with addresses are the ones an operator came for.
              .filter((link) => link.addresses.length > 0 || link.kind !== "ethernet")
              .slice(0, 10)
              .map((link) => [
                link.name,
                `${link.kind}, ${link.oper_state}${
                  link.addresses.length > 0 ? ` — ${link.addresses.join(", ")}` : ""
                }`,
              ])}
          />
        ) : (
          <div className="text-[13px] text-[var(--qz-fg-3)]">
            No interfaces reported for this node.
          </div>
        )}
      </Panel>
    </div>
  );
}

/// A label-and-value list, the shape the machine overview already uses.
function Facts({ rows }: { rows: [string, string][] }) {
  return (
    <div className="flex flex-col gap-[6px]">
      {rows.map(([label, value]) => (
        <div key={label} className="flex items-baseline gap-3">
          <span className="text-[12px] text-[var(--qz-fg-4)]" style={{ minWidth: 120 }}>
            {label}
          </span>
          <span className="text-[13px] text-[var(--qz-fg-1)] break-all">{value}</span>
        </div>
      ))}
    </div>
  );
}

/// One allocation bar. Allocation, not usage: the appliance knows what it
/// has promised machines, and promising is the number an operator plans
/// with — a guest's own idea of how much memory it is touching is the
/// guest's business.
function Meter({
  label,
  detail,
  used,
  total,
}: {
  label: string;
  detail: string;
  used: number;
  total: number;
}) {
  const percent = total > 0 ? Math.min(100, Math.round((used * 100) / total)) : 0;
  const tone =
    percent >= 90 ? "var(--qz-danger)" : percent >= 75 ? "var(--qz-warn)" : "var(--qz-accent)";
  return (
    <div className="flex flex-col gap-[6px]">
      <div className="flex items-baseline gap-3">
        <span className="text-[13px] text-[var(--qz-fg-1)] font-semibold">{label}</span>
        <span className="text-[12px] text-[var(--qz-fg-4)] ml-auto qz-mono">{percent}%</span>
      </div>
      <div
        style={{
          height: 6,
          borderRadius: 3,
          background: "color-mix(in oklab, white 8%, transparent)",
          overflow: "hidden",
        }}
      >
        <div style={{ width: `${percent}%`, height: "100%", background: tone }} />
      </div>
      <span className="text-[12px] text-[var(--qz-fg-3)]">{detail}</span>
    </div>
  );
}

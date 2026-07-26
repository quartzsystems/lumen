"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { AlertTriangle } from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import {
  Bar,
  DashPanel,
  MoreLink,
  PanelEmpty,
  StatTile,
  Status,
  staticRow,
  type Tone,
} from "@/components/dashboard/DashboardBits";
import { deriveAlarms, SEVERITY_TONE, worstSeverity, type Alarm } from "@/lib/alarms";
import { ApiError } from "@/lib/authClient";
import { titleCase } from "@/lib/labels";
import {
  fetchInterfaces,
  fetchPending,
  type LinkKind,
  type LinkView,
  type PendingResponse,
} from "@/lib/networkClient";
import { assignableMemoryMib, fetchNodes, type NodeView } from "@/lib/nodeClient";
import { fetchPools, type PoolView } from "@/lib/storageClient";
import { useVms } from "@/lib/VmContext";
import { fetchRecentTasks, formatMib, type TaskView } from "@/lib/vmClient";

/// Matches lib/VmContext.tsx: often enough that the page feels live, slow
/// enough to cost nothing.
const POLL_MS = 5000;

/// How much of each list a panel shows before sending the operator to the page
/// that owns it in full.
const NETWORK_ROWS = 8;
const LOG_ROWS = 12;

/// The order the networks panel reads in: the link the appliance is managed
/// over, then the ones machines attach to, then the plumbing underneath. A
/// "top" list with no metric to rank by ranks by what an operator looks at
/// first — there are no traffic counters behind this page to sort on, and
/// inventing an order that looks like throughput would be a lie.
const KIND_ORDER: Record<LinkKind, number> = {
  bridge: 0,
  bond: 1,
  vlan: 2,
  ethernet: 3,
  other: 4,
};

/// The console's landing page: what this appliance is running, what is wrong
/// with it, and what has been done to it lately.
///
/// Every number here is read from the subsystem that owns it rather than
/// stored — there is no dashboard service, and nothing on this page is
/// authoritative about anything. Panels whose subsystem does not exist yet say
/// so in the same words the rest of the console uses.
export default function DashboardPage() {
  const router = useRouter();
  const { vms, error: vmsError } = useVms();

  const [capacity, setCapacity] = useState<NodeView[] | null>(null);
  const [links, setLinks] = useState<LinkView[] | null>(null);
  const [pending, setPending] = useState<PendingResponse | null>(null);
  const [pools, setPools] = useState<PoolView[] | null>(null);
  const [tasks, setTasks] = useState<TaskView[] | null>(null);
  /// Which subsystems could not be read this poll. Named rather than counted:
  /// "storage could not be read" is actionable and "0 pools" is a lie.
  const [unread, setUnread] = useState<string[]>([]);

  // Settled rather than all: a dashboard is five independent readings, and one
  // subsystem being down must not blank the four that are up.
  const load = useCallback(async () => {
    const [nodes, interfaces, staged, storage, log] = await Promise.allSettled([
      fetchNodes(),
      fetchInterfaces(),
      fetchPending(),
      fetchPools(),
      fetchRecentTasks(LOG_ROWS),
    ]);

    const failed: string[] = [];
    const take = <T,>(
      result: PromiseSettledResult<T>,
      subsystem: string,
      apply: (value: T) => void,
    ) => {
      if (result.status === "fulfilled") {
        apply(result.value);
        return;
      }
      // A 401 has already redirected to /login; there is nothing to report and
      // this page is about to be unmounted anyway.
      if (result.reason instanceof ApiError && result.reason.status === 401) return;
      failed.push(subsystem);
    };

    take(nodes, "the node", (value) => setCapacity(value.nodes));
    take(interfaces, "networking", (value) =>
      setLinks(value.nodes.flatMap((node) => node.interfaces)),
    );
    take(staged, "staged network changes", setPending);
    take(storage, "storage", (value) => setPools(value.nodes.flatMap((node) => node.pools)));
    take(log, "the log", (value) => setTasks(value.tasks));
    setUnread(failed);
  }, []);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const running = vms.filter((vm) => vm.state === "running").length;
  const bridges = useMemo(() => (links ?? []).filter((link) => link.kind === "bridge"), [links]);
  const bridgesUp = bridges.filter((link) => link.oper_state === "activated").length;

  const alarms = useMemo(
    () =>
      deriveAlarms({
        vms,
        pools: pools ?? undefined,
        links: links ?? undefined,
        pending,
        nodes: capacity ?? undefined,
      }),
    [vms, pools, links, pending, capacity],
  );
  const worst = worstSeverity(alarms);

  const topLinks = useMemo(
    () =>
      [...(links ?? [])]
        .sort((a, b) => {
          if (a.management !== b.management) return a.management ? -1 : 1;
          if (KIND_ORDER[a.kind] !== KIND_ORDER[b.kind])
            return KIND_ORDER[a.kind] - KIND_ORDER[b.kind];
          return a.name.localeCompare(b.name);
        })
        .slice(0, NETWORK_ROWS),
    [links],
  );

  // The log records identifiers, not names, because a machine can be renamed
  // and its history must not be rewritten when it is. The name is looked up
  // for display only, and a machine that has since been removed keeps its
  // number.
  const nameOf = useMemo(() => {
    const names = new Map(vms.map((vm) => [vm.vmid, vm.name]));
    return (vmid: number) => names.get(vmid) ?? null;
  }, [vms]);

  return (
    <Page>
      <PageHeader
        title="Dashboard"
        description="What this appliance is running, and what has been done to it."
      />
      <PageBody>
        <div className="flex flex-col gap-4">
          {/* One line per subsystem that could not be read. A dashboard that
              renders a stale panel as though it were current is worse than one
              that says which reading it is missing. */}
          {(vmsError || unread.length > 0) && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="flex flex-col gap-1 text-[13px] text-[var(--qz-fg-2)]">
                {vmsError && <span>{vmsError}</span>}
                {unread.length > 0 && (
                  <span>
                    Could not read {unread.join(", ")} — those panels are showing the last reading
                    that worked.
                  </span>
                )}
              </div>
            </div>
          )}

          <div
            className="grid gap-4"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(190px, 1fr))" }}
          >
            <StatTile
              label="Virtual Machines"
              href="/virtual-machines"
              value={String(running)}
              of={String(vms.length)}
              // Machines running is a good number to be high, so it never goes
              // amber — the meter's colour has to mean the same thing on every
              // tile, and here more is better.
              bar={<Bar percent={share(running, vms.length)} tone="ok" />}
            />
            <StatTile
              label="Networks"
              href="/networking/interfaces"
              value={links === null ? "—" : String(bridgesUp)}
              of={links === null ? undefined : String(bridges.length)}
              bar={<Bar percent={share(bridgesUp, bridges.length)} tone="ok" />}
              note="Bridges, which is what a machine attaches to."
            />
            <StatTile
              label="Clusters"
              href="/infrastructure/clusters"
              value="—"
              note="Clustering has not landed yet."
            />
            <StatTile
              label="Nodes"
              href="/infrastructure/nodes"
              value={capacity === null ? "—" : String(capacity.length)}
              of={capacity === null ? undefined : String(capacity.length)}
              bar={<Bar percent={capacity === null ? 0 : 100} tone="ok" />}
            />
            <StatTile
              label="Alarms"
              value={String(alarms.length)}
              // No proportion to show — an alarm count has no denominator. The
              // bar carries the worst severity instead, which is the thing
              // worth seeing from across a room.
              bar={<Bar percent={worst ? 100 : 0} tone={worst ? SEVERITY_TONE[worst] : "muted"} />}
              note={alarms.length === 0 ? "Nothing is wrong right now." : undefined}
            />
          </div>

          {/* Only when there is something to say. Nothing wrong means the page
              is tiles and panels, exactly as it is designed to be read. */}
          {alarms.length > 0 && (
            <div className="flex flex-col gap-2">
              {alarms.map((alarm) => (
                <AlarmRow key={alarm.id} alarm={alarm} />
              ))}
            </div>
          )}

          <div
            className="grid gap-4 items-start"
            style={{ gridTemplateColumns: "repeat(auto-fit, minmax(430px, 1fr))" }}
          >
            <div className="flex flex-col gap-4 min-w-0">
              <DashPanel title="Top Compute Clusters">
                <PanelEmpty>
                  This appliance is not part of a cluster. Clusters land here when clustering does.
                </PanelEmpty>
              </DashPanel>

              <DashPanel
                title="Compute Nodes"
                action={<MoreLink href="/infrastructure/nodes">Nodes</MoreLink>}
              >
                {capacity === null ? (
                  <PanelEmpty>Reading the node…</PanelEmpty>
                ) : (
                  <table className="qz-table">
                    <thead>
                      <tr>
                        <th style={{ width: 110 }}>Status</th>
                        <th>Name</th>
                        <th style={{ width: 100 }}>Machines</th>
                        <th style={{ width: 150 }}>Cores</th>
                        <th style={{ width: 170 }}>RAM</th>
                      </tr>
                    </thead>
                    <tbody>
                      {capacity.map((node) => (
                        <NodeRow key={node.node} node={node} />
                      ))}
                    </tbody>
                  </table>
                )}
              </DashPanel>
            </div>

            <DashPanel
              title="Top Networks"
              action={<MoreLink href="/networking/interfaces">Interfaces</MoreLink>}
            >
              {links === null ? (
                <PanelEmpty>Reading the network configuration…</PanelEmpty>
              ) : topLinks.length === 0 ? (
                <PanelEmpty>This node has no interfaces configured.</PanelEmpty>
              ) : (
                <table className="qz-table">
                  <thead>
                    <tr>
                      <th style={{ width: 120 }}>Status</th>
                      <th style={{ width: 150 }}>Name</th>
                      <th style={{ width: 130 }}>Type</th>
                      <th>Address</th>
                    </tr>
                  </thead>
                  <tbody>
                    {topLinks.map((link) => (
                      <LinkRow key={link.name} link={link} />
                    ))}
                  </tbody>
                </table>
              )}
            </DashPanel>
          </div>

          <DashPanel title="Logs">
            {tasks === null ? (
              <PanelEmpty>Reading the log…</PanelEmpty>
            ) : tasks.length === 0 ? (
              <PanelEmpty>Nothing has been done on this node yet.</PanelEmpty>
            ) : (
              <table className="qz-table">
                <thead>
                  <tr>
                    <th style={{ width: 90 }}>Status</th>
                    <th style={{ width: 180 }}>Time</th>
                    <th style={{ width: 170 }}>Machine</th>
                    <th style={{ width: 120 }}>Action</th>
                    <th style={{ width: 150 }}>User</th>
                    <th>Description</th>
                  </tr>
                </thead>
                <tbody>
                  {tasks.map((task) => (
                    <tr
                      key={task.id}
                      // The whole of a machine's history is one click away, on
                      // the machine it belongs to.
                      onClick={() =>
                        router.push(`/virtual-machines?vm=${task.vmid}&section=tasks`)
                      }
                    >
                      <td>
                        {task.status === "ok" ? (
                          <span className="badge badge-ok">OK</span>
                        ) : (
                          <span className="badge badge-crit" title={task.error ?? undefined}>
                            error
                          </span>
                        )}
                      </td>
                      <td className="mono">{formatTime(task.time)}</td>
                      <td className="mono truncate" title={nameOf(task.vmid) ?? undefined}>
                        {nameOf(task.vmid) ?? task.vmid}
                      </td>
                      <td className="mono">{task.action}</td>
                      <td className="mono">{task.user}</td>
                      <td>
                        {task.detail}
                        {task.error && <span className="qz-dim"> — {task.error}</span>}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </DashPanel>
        </div>
      </PageBody>
    </Page>
  );
}

// --- rows --------------------------------------------------------------------

/// One derived alarm, in the same shape every other warning in this console
/// wears — and a link, because an alarm nobody can act on is decoration.
///
/// `.callout` top-aligns its icon and is declared outside Tailwind's layer, so
/// a utility cannot re-align it. The `mt-[1px]` on the icon is the console's
/// existing answer to that, and this uses it rather than fighting it.
function AlarmRow({ alarm }: { alarm: Alarm }) {
  const critical = alarm.severity === "critical";
  return (
    <Link href={alarm.href} className={`callout callout-${critical ? "crit" : "warn"} no-underline`}>
      <AlertTriangle
        size={17}
        className={`flex-shrink-0 mt-[1px] ${critical ? "text-[var(--qz-danger)]" : "text-[var(--qz-warn)]"}`}
      />
      <div className="flex-1 text-[13px] text-[var(--qz-fg-2)] min-w-0">{alarm.summary}</div>
      <span className="badge badge-muted flex-shrink-0">{alarm.source}</span>
    </Link>
  );
}

function NodeRow({ node }: { node: NodeView }) {
  // Against what a machine may actually be given, not against every byte the
  // node has: the reserve is never available, and a meter that counts it reads
  // as healthier than the node is.
  const assignable = assignableMemoryMib(node);
  return (
    <tr style={staticRow}>
      <td>
        <Status tone="ok" label="Online" />
      </td>
      <td className="mono truncate" title={node.hypervisor_version ?? undefined}>
        {node.node}
      </td>
      <td className="mono">
        {node.running} / {node.machines}
      </td>
      <td>
        <Usage
          used={String(node.used_vcpus)}
          of={String(node.cpus)}
          percent={share(node.used_vcpus, node.cpus)}
        />
      </td>
      <td>
        <Usage
          used={formatMib(node.used_memory_mib)}
          of={formatMib(assignable)}
          percent={share(node.used_memory_mib, assignable)}
        />
      </td>
    </tr>
  );
}

function LinkRow({ link }: { link: LinkView }) {
  const tone: Tone = !link.present
    ? "muted"
    : link.oper_state === "activated"
      ? "ok"
      : link.oper_state === "activating"
        ? "warn"
        : "muted";
  const address = link.addresses.join(", ");
  return (
    <tr style={staticRow}>
      <td>
        <Status tone={tone} label={titleCase(link.present ? link.oper_state : "staged")} />
      </td>
      <td className="mono truncate" title={link.name}>
        {link.name}
      </td>
      <td>
        <span className="whitespace-nowrap">
          {titleCase(link.kind)}
          {link.management && <span className="badge badge-info ml-2">mgmt</span>}
        </span>
      </td>
      <td className="mono truncate" title={address}>
        {address || <span className="qz-dim">—</span>}
      </td>
    </tr>
  );
}

/// A figure over its meter — what the compute panel's Cores and RAM cells are.
function Usage({ used, of, percent }: { used: string; of: string; percent: number }) {
  return (
    <div className="flex flex-col gap-[6px] min-w-0">
      <span className="qz-mono text-[12px] whitespace-nowrap">
        {used} <span className="text-[var(--qz-fg-4)]">/ {of}</span>
      </span>
      <Bar percent={percent} />
    </div>
  );
}

// --- helpers -----------------------------------------------------------------

/// A percentage that answers 0 rather than NaN when there is nothing to divide
/// by — a node with no processors reported is a node with an empty meter, not
/// a broken one.
const share = (part: number, whole: number): number => (whole > 0 ? (part / whole) * 100 : 0);

/// When, as an operator reads it: absolute and in their own locale, because
/// "3 minutes ago" is useless in an incident review. Matches
/// components/vm/VmTasks.tsx, which shows the same records per machine.
const formatTime = (unixSecs: number): string =>
  new Date(unixSecs * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
  });

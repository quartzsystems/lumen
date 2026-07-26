import type { LinkView, PendingResponse } from "@/lib/networkClient";
import { assignableMemoryMib, type NodeView } from "@/lib/nodeClient";
import type { PoolView } from "@/lib/storageClient";
import type { VmView } from "@/lib/vmClient";

// What is wrong with this appliance right now, worked out from what the other
// subsystems already report.
//
// There is no alarm store in the control plane, and this is deliberately not
// pretending to be one: nothing here is raised, cleared, acknowledged, or
// remembered. Every alarm below is a *reading* of state the API answers with
// on every poll — a crashed machine, a pool that is not healthy, a network
// change that will roll itself back. Fix the condition and the alarm is gone
// on the next poll, because there was never anything but the condition.
//
// The rule for adding one: it must be derivable from a single field, it must
// be something an operator would act on, and it must clear itself. A
// condition that is normal for half of every working day (an adapter with no
// cable in it, a form half filled in) is not an alarm, it is a fact, and it
// belongs on the page that owns it.

export type AlarmSeverity = "critical" | "warning";

export interface Alarm {
  /// Stable across polls, so React keeps the row rather than remounting it.
  id: string;
  severity: AlarmSeverity;
  /// Which subsystem noticed, in the words the left nav uses.
  source: string;
  /// What is wrong, in one line an operator can act on.
  summary: string;
  /// Where to go to do something about it.
  href: string;
}

/// Everything the derivation reads. All optional: the dashboard renders before
/// its requests land, and a subsystem that could not be read must not be
/// reported as healthy — it is simply not consulted.
export interface AlarmSources {
  vms?: VmView[];
  pools?: PoolView[];
  links?: LinkView[];
  pending?: PendingResponse | null;
  nodes?: NodeView[];
}

/// Past this much of a pool spoken for, the number stops being reassuring.
const POOL_WARN_PERCENT = 90;
const POOL_CRIT_PERCENT = 95;

export function deriveAlarms({ vms, pools, links, pending, nodes }: AlarmSources): Alarm[] {
  const alarms: Alarm[] = [];

  // A machine the hypervisor reports as crashed. Nothing else in the console
  // is loud about this — the table shows a red badge on one row among many.
  for (const vm of vms ?? []) {
    if (vm.state !== "crashed") continue;
    alarms.push({
      id: `vm-crashed-${vm.vmid}`,
      severity: "critical",
      source: "Virtual Machines",
      summary: `${vm.name} has crashed.`,
      href: `/virtual-machines?vm=${vm.vmid}&section=overview`,
    });
  }

  for (const pool of pools ?? []) {
    // Health first: a faulted pool is not a capacity problem and must not be
    // reported as one.
    if (pool.health !== "online") {
      const lost = pool.health === "faulted" || pool.health === "unavail" || pool.health === "removed";
      alarms.push({
        id: `pool-health-${pool.name}`,
        severity: lost ? "critical" : "warning",
        source: "Storage",
        summary: `Pool ${pool.name} is ${pool.health}.`,
        href: "/storage",
      });
      continue;
    }
    if (pool.used_percent >= POOL_WARN_PERCENT) {
      alarms.push({
        id: `pool-full-${pool.name}`,
        severity: pool.used_percent >= POOL_CRIT_PERCENT ? "critical" : "warning",
        source: "Storage",
        summary: `Pool ${pool.name} is ${Math.round(pool.used_percent)}% full.`,
        href: "/storage",
      });
    }
  }

  // An applied network change nobody has confirmed. The most urgent thing the
  // appliance can be doing: it undoes itself when the timer runs out, whether
  // or not that is what anyone wanted.
  if (pending?.checkpoint) {
    alarms.push({
      id: "network-unconfirmed",
      severity: "critical",
      source: "Networking",
      summary: "A network change will roll back unless it is confirmed.",
      href: "/networking/interfaces",
    });
  }

  // Machines need a bridge to attach to, and the management interface is the
  // one that has the address. The Networking overview says this too; the
  // dashboard is where somebody who has not been to that page will see it.
  const management = (links ?? []).find((link) => link.management);
  if (management && management.kind !== "bridge") {
    alarms.push({
      id: "management-not-bridged",
      severity: "warning",
      source: "Networking",
      summary: `${management.name} carries the management address but is not a bridge, so machines cannot attach to it.`,
      href: "/networking/interfaces",
    });
  }

  // Both node alarms point at the machines rather than at the node: the node's
  // size is not something an operator can change from the console, and what
  // they would actually do about either is stop or resize a machine.
  for (const node of nodes ?? []) {
    // More memory handed out than the node has left after its own reserve.
    // The hypervisor allows it; the machines that lose the race do not start.
    const assignable = assignableMemoryMib(node);
    if (assignable > 0 && node.used_memory_mib > assignable) {
      alarms.push({
        id: `node-memory-${node.node}`,
        severity: "critical",
        source: "Infrastructure",
        summary: `${node.node} has more memory in use than it can spare for machines.`,
        href: "/virtual-machines",
      });
    }
    // Overcommitted processors are a slowdown, not a failure — every guest
    // gets less of one than it thinks it has.
    if (node.cpus > 0 && node.used_vcpus > node.cpus) {
      alarms.push({
        id: `node-cpu-${node.node}`,
        severity: "warning",
        source: "Infrastructure",
        summary: `${node.node} has ${node.used_vcpus} processors assigned across ${node.cpus} it actually has.`,
        href: "/virtual-machines",
      });
    }
  }

  // Critical first, then in the order they were derived — which groups them by
  // subsystem without anything having to say so.
  return alarms.sort((a, b) => severityRank(b.severity) - severityRank(a.severity));
}

const severityRank = (severity: AlarmSeverity): number => (severity === "critical" ? 1 : 0);

/// The badge and meter tone a set of alarms wears: the worst one in it.
export const worstSeverity = (alarms: Alarm[]): AlarmSeverity | null =>
  alarms.some((alarm) => alarm.severity === "critical")
    ? "critical"
    : alarms.length > 0
      ? "warning"
      : null;

export const SEVERITY_TONE: Record<AlarmSeverity, "crit" | "warn"> = {
  critical: "crit",
  warning: "warn",
};

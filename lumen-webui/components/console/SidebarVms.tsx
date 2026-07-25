"use client";

import Link from "next/link";
import { useMemo, useState } from "react";
import { Search } from "lucide-react";
import { useVms } from "@/lib/VmContext";
import { STATE_TONE, type VmView } from "@/lib/vmClient";

/// Past this many machines a list is something you search rather than read,
/// so the filter box appears.
const FILTER_THRESHOLD = 15;

export const vmHref = (vmid: number, section = "overview"): string =>
  `/virtual-machines?vm=${vmid}&section=${section}`;

/// The live machine list under the Virtual Machines nav item.
///
/// Deliberately not part of `lib/nav.ts`: that file describes itself as the
/// single source of truth for navigation, and navigation is static. Machines
/// are runtime data — they appear, disappear, and change state while the
/// operator is looking at them — so they are composed in here instead, against
/// the same list the command palette and the page itself read.
export function SidebarVms() {
  const { vms, loading, error, selected } = useVms();
  const [query, setQuery] = useState("");

  const shown = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return vms;
    return vms.filter(
      (vm) => vm.name.toLowerCase().includes(needle) || String(vm.vmid).includes(needle),
    );
  }, [vms, query]);

  if (error) {
    return (
      <div className="px-[10px] py-[6px] text-[11.5px] text-[var(--qz-warn)]" title={error}>
        Machines unavailable
      </div>
    );
  }
  if (loading && vms.length === 0) {
    return <div className="px-[10px] py-[6px] text-[11.5px] text-[var(--qz-fg-4)]">Reading…</div>;
  }
  if (vms.length === 0) {
    return (
      <div className="px-[10px] py-[6px] text-[11.5px] text-[var(--qz-fg-4)]">No machines yet</div>
    );
  }

  return (
    <div className="flex flex-col gap-[2px]">
      {vms.length > FILTER_THRESHOLD && (
        <div className="relative mb-[2px]">
          <Search
            size={12}
            className="absolute left-[8px] top-1/2 -translate-y-1/2 text-[var(--qz-fg-4)]"
          />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Filter…"
            aria-label="Filter machines"
            className="sidebar-vm-filter"
          />
        </div>
      )}
      <div className="sidebar-vm-list">
        {shown.map((vm) => (
          <SidebarVm key={vm.vmid} vm={vm} active={vm.vmid === selected} />
        ))}
        {shown.length === 0 && (
          <div className="px-[10px] py-[6px] text-[11.5px] text-[var(--qz-fg-4)]">No matches</div>
        )}
      </div>
    </div>
  );
}

function SidebarVm({ vm, active }: { vm: VmView; active: boolean }) {
  return (
    <Link
      href={vmHref(vm.vmid)}
      className={`sidebar-vm${active ? " sidebar-vm-active" : ""}`}
      title={`${vm.vmid} · ${vm.name} · ${vm.state}`}
    >
      <span className={`state-dot state-dot-${STATE_TONE[vm.state]}`} aria-hidden />
      <span className="sidebar-vm-id">{vm.vmid}</span>
      <span className="sidebar-vm-name">{vm.name}</span>
      {/* Inert until clustering: one node needs no disambiguation, but the
          column has to exist before there are two. */}
      <span className="sidebar-vm-node" title={`on ${vm.node}`}>
        {vm.node}
      </span>
    </Link>
  );
}

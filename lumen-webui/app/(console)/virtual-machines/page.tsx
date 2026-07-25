"use client";

import { Suspense, useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import {
  AlertTriangle,
  Archive,
  Camera,
  Cpu,
  LayoutDashboard,
  ListChecks,
  Monitor,
  Plus,
  Settings2,
  Trash2,
} from "lucide-react";
import { Page, PageBody, PageHeader } from "@/components/PageHeader";
import { DataTable, Dash, type Column, type FilterDef } from "@/components/console/DataTable";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ModalFooter } from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { CreateVmDialog } from "@/components/vm/CreateVmDialog";
import { LifecycleControls } from "@/components/vm/LifecycleControls";
import { VmConsole } from "@/components/vm/VmConsole";
import { StateBadge, Tags } from "@/components/vm/VmBits";
import { VmHardware } from "@/components/vm/VmHardware";
import { VmOptions } from "@/components/vm/VmOptions";
import { VmOverview, VmStubSection } from "@/components/vm/VmOverview";
import { useConsole } from "@/lib/ConsoleContext";
import { useSecondaryNav } from "@/lib/SecondaryNavContext";
import { useVms } from "@/lib/VmContext";
import {
  deleteVm,
  formatBytes,
  formatMib,
  formatUptime,
  STATE_LABEL,
  type VmView,
} from "@/lib/vmClient";

/// The detail page's sections. Snapshots, backups, and tasks are stubs at this
/// stage; they are listed anyway so the shape of the page is visible and so
/// filling them in stays purely additive.
const SECTIONS = [
  { id: "overview", label: "Overview", icon: LayoutDashboard },
  { id: "console", label: "Console", icon: Monitor },
  { id: "hardware", label: "Hardware", icon: Cpu },
  { id: "snapshots", label: "Snapshots", icon: Camera },
  { id: "backups", label: "Backups", icon: Archive },
  { id: "options", label: "Options", icon: Settings2 },
  { id: "tasks", label: "Tasks", icon: ListChecks },
] as const;

type SectionId = (typeof SECTIONS)[number]["id"];

const isSection = (value: string | null): value is SectionId =>
  SECTIONS.some((section) => section.id === value);

/// The static export has no dynamic segments, so which machine is open and
/// which section it is showing live in the query string. `useSearchParams`
/// reads them, and reading them during a prerender pass is what the Suspense
/// boundary below is for — without it the export fails with an opaque error.
export default function VirtualMachinesPage() {
  return (
    <Suspense
      fallback={
        <Page>
          <PageHeader title="Virtual Machines" />
          <PageBody>
            <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the machines…</div>
          </PageBody>
        </Page>
      }
    >
      <VirtualMachines />
    </Suspense>
  );
}

function VirtualMachines() {
  const params = useSearchParams();
  const router = useRouter();
  const { setToast } = useConsole();
  const { nodes, vms, loading, error, refresh, setPaused, setSelected } = useVms();

  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<VmView | null>(null);
  const [busy, setBusy] = useState(false);

  const vmParam = params.get("vm");
  const requested = vmParam === null || vmParam === "" ? null : Number(vmParam);
  const sectionParam = params.get("section");
  const section: SectionId = isSection(sectionParam) ? sectionParam : "overview";
  const selected = useMemo(
    () => (requested === null ? null : (vms.find((vm) => vm.vmid === requested) ?? null)),
    [vms, requested],
  );

  // Tell the sidebar which machine is open, so it can highlight the row. The
  // sidebar cannot read the query string itself without putting the whole
  // console layout behind a Suspense boundary — see lib/VmContext.tsx.
  useEffect(() => {
    setSelected(requested);
    return () => setSelected(null);
  }, [requested, setSelected]);

  // Polling pauses while a dialog is open, so a refresh cannot yank the form
  // out from under the operator mid-edit.
  useEffect(() => {
    setPaused(creating || deleting !== null);
    return () => setPaused(false);
  }, [creating, deleting, setPaused]);

  const go = useCallback(
    (vmid: number | null, to: SectionId = "overview") => {
      router.push(
        vmid === null ? "/virtual-machines" : `/virtual-machines?vm=${vmid}&section=${to}`,
      );
    },
    [router],
  );

  // The context column, filled only while a machine is open — so the list view
  // keeps the plain two-column shell.
  useSecondaryNav(
    () =>
      selected ? (
        <>
          <div className="context-nav-header">
            <div className="context-nav-title" title={selected.name}>
              {selected.name}
            </div>
            <div className="context-nav-sub">
              <span className="qz-mono text-[11px] text-[var(--qz-fg-4)]">{selected.vmid}</span>
              <StateBadge state={selected.state} />
            </div>
          </div>
          <nav className="context-nav-items">
            {SECTIONS.map(({ id, label, icon: Icon }) => (
              <Link
                key={id}
                href={`/virtual-machines?vm=${selected.vmid}&section=${id}`}
                className={`context-nav-item${id === section ? " context-nav-item-active" : ""}`}
              >
                <Icon size={15} />
                <span>{label}</span>
              </Link>
            ))}
          </nav>
        </>
      ) : null,
    [selected?.vmid, selected?.name, selected?.state, section],
  );

  const afterAction = useCallback(
    async (message: string) => {
      setToast(message);
      await refresh();
    },
    [setToast, refresh],
  );

  const remove = async (vm: VmView, purge: boolean, acknowledge: boolean) => {
    setBusy(true);
    try {
      const response = await deleteVm(vm.vmid, purge, acknowledge);
      setDeleting(null);
      setToast(
        response.removed_volumes.length > 0
          ? `${response.name} removed, along with ${response.removed_volumes.join(", ")}.`
          : response.kept_volumes.length > 0
            ? `${response.name} removed. ${response.kept_volumes.join(", ")} kept.`
            : `${response.name} removed.`,
      );
      go(null);
      await refresh();
    } catch (err) {
      setToast(err instanceof Error ? err.message : "Could not remove the machine.");
    } finally {
      setBusy(false);
    }
  };

  // A machine that was open and has since been removed — by this console or by
  // anything else — must not leave the page showing a ghost.
  const missing = requested !== null && !loading && selected === null;

  return (
    <Page>
      <PageHeader
        title={selected ? selected.name : "Virtual Machines"}
        description={
          selected
            ? (selected.description ?? `Machine ${selected.vmid} on ${selected.node}.`)
            : "Machines defined on this node, and the disks and adapters they run on."
        }
        // Creating is a control on the list, not on the page: it belongs in
        // the table's toolbar next to Columns and Refresh, the way Interfaces
        // has it. The header keeps what acts on the machine that is open.
        actions={
          selected ? (
            <div className="flex items-center gap-2">
              <LifecycleControls vm={selected} busy={busy} compact onDone={afterAction} />
              <span title={selected.actions.delete.reason}>
                <Button
                  kind="danger"
                  icon={Trash2}
                  disabled={busy || !selected.actions.delete.allowed}
                  onClick={() => setDeleting(selected)}
                >
                  Remove
                </Button>
              </span>
            </div>
          ) : undefined
        }
      />

      <PageBody>
        <div className="flex flex-col gap-4">
          {error && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
            </div>
          )}

          {missing && (
            <div className="callout callout-warn">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="flex-1 text-[13px] text-[var(--qz-fg-2)]">
                Machine {requested} is not on this node.
              </div>
              <Button kind="secondary" onClick={() => go(null)}>
                Back to all machines
              </Button>
            </div>
          )}

          {selected ? (
            <VmSection vm={selected} section={section} busy={busy} onChanged={afterAction} />
          ) : (
            !missing && (
              <>
                {loading && vms.length === 0 && !error && (
                  <div className="text-[13px] text-[var(--qz-fg-4)]">Reading the machines…</div>
                )}
                {nodes.map((node) => (
                  <section key={node.node} className="flex flex-col gap-2">
                    {/* One appliance is the usual case, and naming it then is
                        noise. A cluster is not, and then every table needs
                        saying whose. */}
                    {nodes.length > 1 && (
                      <h2
                        className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0"
                        style={{ fontFamily: "var(--qz-font-mono)" }}
                      >
                        {node.node}
                      </h2>
                    )}
                    <VmTable
                      rows={node.vms}
                      busy={busy}
                      toolbar={
                        <Button kind="primary" size="sm" icon={Plus} onClick={() => setCreating(true)}>
                          Create
                        </Button>
                      }
                      onRefresh={refresh}
                      onOpen={(vm) => go(vm.vmid)}
                      onAction={afterAction}
                    />
                  </section>
                ))}
              </>
            )
          )}
        </div>
      </PageBody>

      {creating && (
        <CreateVmDialog
          onClose={() => setCreating(false)}
          onCreated={async (vm) => {
            setCreating(false);
            setToast(`${vm.name} created as machine ${vm.vmid}.`);
            await refresh();
            go(vm.vmid);
          }}
        />
      )}

      {deleting && (
        <DeleteVmDialog
          vm={deleting}
          busy={busy}
          onCancel={() => setDeleting(null)}
          onConfirm={(purge, acknowledge) => remove(deleting, purge, acknowledge)}
        />
      )}
    </Page>
  );
}

function VmSection({
  vm,
  section,
  busy,
  onChanged,
}: {
  vm: VmView;
  section: SectionId;
  busy: boolean;
  onChanged: (message: string) => Promise<void>;
}) {
  switch (section) {
    case "overview":
      return <VmOverview vm={vm} busy={busy} onAction={onChanged} />;
    case "hardware":
      return <VmHardware vm={vm} busy={busy} onChanged={onChanged} />;
    case "options":
      // Keyed on the machine, so opening a different one starts from its own
      // values rather than the previous machine's half-edited form.
      return <VmOptions key={vm.vmid} vm={vm} busy={busy} onChanged={onChanged} />;
    case "console":
      return <VmConsole vm={vm} busy={busy} onAction={onChanged} />;
    case "snapshots":
      return (
        <VmStubSection title="Snapshots" note="taken from the pool the machine's disks live on." />
      );
    case "backups":
      return <VmStubSection title="Backups" note="scheduled and restored from here." />;
    case "tasks":
      return <VmStubSection title="Tasks" note="a record of what has been done to this machine." />;
  }
}

// --- the table ---------------------------------------------------------------

const columns: Column<VmView>[] = [
  {
    key: "vmid",
    header: "ID",
    value: (vm) => vm.vmid,
    sortable: true,
    mono: true,
    width: 80,
  },
  {
    key: "name",
    header: "Name",
    value: (vm) => vm.name,
    sortable: true,
    width: 180,
    render: (vm) => (
      <span
        className="text-[var(--qz-fg-1)] font-semibold truncate"
        style={{ fontFamily: "var(--qz-font-mono)" }}
      >
        {vm.name}
      </span>
    ),
  },
  {
    key: "node",
    header: "Node",
    value: (vm) => vm.node,
    mono: true,
    sortable: true,
    width: 120,
  },
  {
    key: "state",
    header: "State",
    value: (vm) => STATE_LABEL[vm.state],
    render: (vm) => <StateBadge state={vm.state} />,
    sortable: true,
    width: 110,
  },
  {
    key: "vcpus",
    header: "vCPU",
    value: (vm) => vm.vcpus,
    mono: true,
    sortable: true,
    width: 80,
  },
  {
    key: "memory",
    header: "Memory",
    value: (vm) => vm.memory_mib,
    render: (vm) => <span className="qz-mono">{formatMib(vm.memory_mib)}</span>,
    sortable: true,
    width: 110,
  },
  {
    key: "disk",
    header: "Disk",
    value: (vm) => vm.total_disk_bytes,
    render: (vm) =>
      vm.total_disk_bytes > 0 ? (
        <span className="qz-mono">{formatBytes(vm.total_disk_bytes)}</span>
      ) : (
        <Dash />
      ),
    sortable: true,
    width: 110,
  },
  {
    key: "uptime",
    header: "Uptime",
    value: (vm) => vm.uptime_secs ?? 0,
    render: (vm) =>
      vm.uptime_secs === null ? (
        <Dash />
      ) : (
        <span className="qz-mono">{formatUptime(vm.uptime_secs)}</span>
      ),
    sortable: true,
    width: 110,
  },
  {
    key: "tags",
    header: "Tags",
    value: (vm) => vm.tags.join(", "),
    render: (vm) => <Tags tags={vm.tags} />,
    width: 180,
  },
];

function VmTable({
  rows,
  busy,
  toolbar,
  onRefresh,
  onOpen,
  onAction,
}: {
  rows: VmView[];
  busy: boolean;
  toolbar?: React.ReactNode;
  onRefresh: () => Promise<void>;
  onOpen: (vm: VmView) => void;
  onAction: (message: string) => Promise<void>;
}) {
  // The drop-downs offer what is actually on this node, not every value the
  // API can produce — a filter for a state the node does not have is dead
  // space.
  const filters: FilterDef<VmView>[] = useMemo(
    () => [
      {
        key: "state",
        label: "State",
        options: Array.from(new Set(rows.map((vm) => STATE_LABEL[vm.state])))
          .sort()
          .map((value) => ({ value, label: value })),
        predicate: (vm, value) => STATE_LABEL[vm.state] === value,
      },
      {
        key: "tag",
        label: "Tag",
        options: Array.from(new Set(rows.flatMap((vm) => vm.tags)))
          .sort()
          .map((value) => ({ value, label: value })),
        predicate: (vm, value) => vm.tags.includes(value),
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
      rowId={(vm) => String(vm.vmid)}
      storageKey="virtual-machines"
      searchPlaceholder="Search machines…"
      emptyMessage="No machines on this node yet."
      onRefresh={onRefresh}
      // Open, the one obvious lifecycle control, and the menu holding the
      // rest. Three controls need more than the default cell, and a fixed
      // layout will not find the room on its own.
      actionsWidth={158}
      actions={(vm) => (
        <div className="inline-flex items-center gap-1 justify-end">
          <Button kind="ghost" size="sm" onClick={() => onOpen(vm)}>
            Open
          </Button>
          <LifecycleControls vm={vm} busy={busy} compact onDone={onAction} />
        </div>
      )}
    />
  );
}

/// Removing a machine is two decisions, not one: whether the definition goes,
/// and whether the data does. The second is off by default and says so.
function DeleteVmDialog({
  vm,
  busy,
  onCancel,
  onConfirm,
}: {
  vm: VmView;
  busy: boolean;
  onCancel: () => void;
  onConfirm: (purge: boolean, acknowledge: boolean) => void;
}) {
  const [purge, setPurge] = useState(false);
  const [acked, setAcked] = useState(false);
  const running = vm.state === "running";
  const needsAck = purge || running;

  return (
    <ModalShell onClose={onCancel}>
      <ModalHeader
        title={`Remove ${vm.name}?`}
        subtitle={`Machine ${vm.vmid} on ${vm.node}.`}
        onClose={onCancel}
      />
      <div className="flex flex-col gap-4">
        {running && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This machine is running. Removing it stops it immediately, without the guest being
              asked.
            </div>
          </div>
        )}
        <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
          The definition is removed from the node. Its volumes are kept unless you say otherwise —
          removing a machine is not the same decision as destroying its data.
        </p>
        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={purge} onChange={setPurge} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            Also destroy its {vm.disks.length === 1 ? "disk" : "disks"}
          </span>
        </label>
        {needsAck && (
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <input
              type="checkbox"
              checked={acked}
              onChange={(e) => setAcked(e.target.checked)}
              style={{ accentColor: "var(--qz-accent)" }}
            />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              I understand this may lose data.
            </span>
          </label>
        )}
        <ModalFooter
          onCancel={onCancel}
          saving={busy}
          disabled={needsAck && !acked}
          savingLabel="Removing…"
          submitLabel="Remove"
          onSubmit={() => onConfirm(purge, acked)}
        />
      </div>
    </ModalShell>
  );
}

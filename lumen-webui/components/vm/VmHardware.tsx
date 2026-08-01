"use client";

import { useEffect, useState } from "react";
import {
  ArrowRightLeft,
  Box,
  Cable,
  CircuitBoard,
  Clock,
  Cpu,
  Disc,
  HardDrive,
  MemoryStick,
  Monitor,
  Network,
  Pencil,
  Plus,
  type LucideIcon,
} from "lucide-react";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ErrorText, Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { DataTable, type Column } from "@/components/console/DataTable";
import { RowActions } from "@/components/console/RowActions";
import { fetchEnvironment } from "@/lib/clusterClient";
import { fetchPooledStorage, type PooledStorageView } from "@/lib/poolClient";
import { fetchInterfaces } from "@/lib/networkClient";
import { fetchPools, type PoolView } from "@/lib/storageClient";
import {
  attachDisk,
  attachNic,
  cpuModelLabel,
  detachDisk,
  detachNic,
  fetchCpuModels,
  formatBytes,
  formatMib,
  migrateVm,
  updateVm,
  validationErrorsOf,
  VIDEO_HINT,
  VIDEO_LABEL,
  type CpuModel,
  type CpuModels,
  type DiskBus,
  type NicModel,
  type VideoModel,
  type VmPatch,
  type VmUpdateResponse,
  type VmView,
} from "@/lib/vmClient";

/// The one string that tells a pooled, replicated disk from a local one.
const isReplicatedSource = (source: string) => source.startsWith("/dev/ublkb");

const numberOf = (text: string): number | undefined => {
  const trimmed = text.trim();
  if (trimmed === "") return undefined;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : undefined;
};

/// One row of the hardware table: a device the machine has, and how it is
/// configured, the way Proxmox lays the same page out.
interface HardwareRow {
  key: string;
  icon: LucideIcon;
  device: string;
  /// What search and sort see — the plain text of the configuration cell.
  text: string;
  value: React.ReactNode;
  actions?: React.ReactNode;
}

const hardwareColumns: Column<HardwareRow>[] = [
  {
    key: "device",
    header: "Device",
    value: (row) => row.device,
    width: 220,
    render: (row) => {
      const Icon = row.icon;
      return (
        <span className="inline-flex items-center gap-2 min-w-0">
          <Icon size={15} className="flex-shrink-0 text-[var(--qz-fg-4)]" />
          <span className="text-[var(--qz-fg-1)] font-semibold truncate">{row.device}</span>
        </span>
      );
    },
  },
  {
    key: "configuration",
    header: "Configuration",
    value: (row) => row.text,
    render: (row) => row.value,
  },
];

/// Disks, adapters, and the sizing that only changes when the machine
/// restarts — one table, one row per device, like the hypervisors everyone
/// has already used.
///
/// Every change here reports what actually happened: the backend answers with
/// what the running machine took and what is waiting for a restart, using the
/// hypervisor's own reasons, and that answer is shown verbatim rather than
/// guessed at from the machine's state.
export function VmHardware({
  vm,
  busy,
  onChanged,
}: {
  vm: VmView;
  busy: boolean;
  onChanged: (message: string) => Promise<void> | void;
}) {
  const [adding, setAdding] = useState<"disk" | "nic" | null>(null);
  const [editing, setEditing] = useState<SizingKey | null>(null);
  const [working, setWorking] = useState(false);
  const [migrating, setMigrating] = useState(false);
  /// The pool serving this machine's /dev/ublkb disks — sizes for the
  /// rows, and the member list a migration chooses from.
  const [pooled, setPooled] = useState<PooledStorageView | null>(null);
  const [localNode, setLocalNode] = useState<string | null>(null);

  const hasReplicated = vm.disks.some((disk) => isReplicatedSource(disk.source));
  useEffect(() => {
    if (!hasReplicated) {
      setPooled(null);
      return;
    }
    void (async () => {
      try {
        const response = await fetchPooledStorage();
        setPooled(response.pool);
      } catch {
        setPooled(null);
      }
      try {
        const environment = await fetchEnvironment();
        const home = environment.clusters
          .flatMap((cluster) => cluster.nodes)
          .find((node) => node.local);
        setLocalNode(home?.node ?? null);
      } catch {
        setLocalNode(null);
      }
    })();
  }, [hasReplicated, vm.vmid, vm.disks.length]);

  const pooledSizeOf = (source: string): number | undefined =>
    pooled?.vdisks.find((vdisk) => vdisk.device === source)?.size_bytes;

  // Where the machine could migrate to: every pool member except this one —
  // placement is by content hash, so no member is a better host than
  // another. Empty means the button explains itself.
  const allReplicated =
    vm.disks.length > 0 && vm.disks.every((disk) => isReplicatedSource(disk.source));
  const migrateTargets =
    allReplicated && pooled
      ? pooled.members.map((member) => member.name).filter((name) => name !== localNode)
      : [];

  const report = async (response: VmUpdateResponse, what: string) => {
    const live = response.applied_live.length;
    const pending = response.pending_reboot.length;
    const message =
      pending > 0
        ? `${what}. ${response.pending_reboot.join("; ")}`
        : live > 0
          ? `${what}, applied to the running machine.`
          : `${what}.`;
    await onChanged(message);
  };

  const run = async (action: () => Promise<VmUpdateResponse>, what: string) => {
    setWorking(true);
    try {
      await report(await action(), what);
    } catch (err) {
      await onChanged(err instanceof Error ? err.message : "Something went wrong.");
    } finally {
      setWorking(false);
    }
  };

  const disabled = busy || working;

  // One dialog per row, each editing exactly the thing its row names. The
  // earlier single dialog edited five settings at once, which meant the task
  // log said "Change memory, firmware, graphics…" for a save that changed one
  // of them — every untouched field still went over the wire.
  const editPencil = (key: SizingKey, label: string) => (
    <EditPencil label={label} disabled={disabled} onClick={() => setEditing(key)} />
  );

  const processors = `${vm.vcpus}${
    vm.topology ? ` (${vm.topology.sockets} sockets, ${vm.topology.cores} cores)` : ""
  } [${cpuModelLabel(vm.cpu_model)}]`;

  const diskText = (disk: (typeof vm.disks)[number]) => {
    // The stored document carries no size for a block-backed disk; the
    // pool knows its vdisks' sizes, so use that rather than printing 0.
    const size = pooledSizeOf(disk.source) ?? disk.size;
    return (
      `${disk.source}, ${disk.bus}, ${formatBytes(size)}` +
      `${disk.cache !== "none" ? `, cache=${disk.cache}` : ""}` +
      `${disk.discard ? ", discard" : ""}` +
      `${disk.boot_index != null ? `, boot=${disk.boot_index}` : ""}`
    );
  };

  const nicText = (nic: (typeof vm.nics)[number]) =>
    `${nic.model}=${nic.id},bridge=${nic.bridge}${nic.vlan_tag != null ? `,tag=${nic.vlan_tag}` : ""}`;

  const rows: HardwareRow[] = [
    {
      key: "memory",
      icon: MemoryStick,
      device: "Memory",
      text: formatMib(vm.memory_mib),
      value: <span className="qz-mono">{formatMib(vm.memory_mib)}</span>,
      actions: editPencil("memory", "memory"),
    },
    {
      key: "processors",
      icon: Cpu,
      device: "Processors",
      text: processors,
      value: <span className="qz-mono">{processors}</span>,
      actions: editPencil("processors", "processors"),
    },
    {
      key: "bios",
      icon: CircuitBoard,
      device: "BIOS",
      text: vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS",
      value: vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS",
      actions: editPencil("bios", "firmware"),
    },
    {
      key: "display",
      icon: Monitor,
      device: "Display",
      /* Not simply the card. A machine whose document has no display device
         reads back as the default one, so printing `video` here told an
         operator "VirtIO GPU" about a machine with no console listener — and
         then the console said there was no screen, which reads as a broken
         appliance rather than as a machine that needs saving. The missing
         thing is the console, not the card, and the remedy is a save (which
         rewrites the document) followed by a full stop and start. */
      text: vm.has_screen ? VIDEO_LABEL[vm.video] : "no console yet",
      value: vm.has_screen ? (
        VIDEO_LABEL[vm.video]
      ) : (
        <span style={{ color: "var(--qz-warn)" }}>
          no console yet{" "}
          <span className="qz-dim">— save this machine, then stop and start it</span>
        </span>
      ),
      actions: editPencil("display", "display"),
    },
    {
      key: "machine",
      icon: Box,
      device: "Machine",
      text: vm.machine,
      value: <span className="qz-mono">{vm.machine}</span>,
      actions: editPencil("machine", "machine type"),
    },
    // Derived, not stored: the controller exists in the machine's document
    // exactly when a disk rides the virtio-scsi bus.
    ...(vm.disks.some((disk) => disk.bus === "virtio-scsi")
      ? [
          {
            key: "scsi-controller",
            icon: Cable,
            device: "SCSI Controller",
            text: "VirtIO SCSI",
            value: "VirtIO SCSI",
          } satisfies HardwareRow,
        ]
      : []),
    ...vm.disks.map((disk) => ({
      key: `disk-${disk.id}`,
      icon: HardDrive,
      device: `Hard Disk (${disk.id})`,
      text: diskText(disk),
      value: (
        <span className="inline-flex items-center gap-2 min-w-0">
          <span className="qz-mono truncate" title={diskText(disk)}>
            {diskText(disk)}
          </span>
          {isReplicatedSource(disk.source) && (
            <span
              className={`badge badge-${
                pooled
                  ? pooled.health === "Healthy"
                    ? "ok"
                    : pooled.health === "Degraded"
                      ? "warn"
                      : "muted"
                  : "muted"
              } flex-shrink-0`}
              title={
                pooled
                  ? `Served by the ${pooled.name} pool, which is ${pooled.health.toLowerCase()}.`
                  : "Served by the cluster's pooled storage."
              }
            >
              {pooled ? `Pooled · ${pooled.health.toLowerCase()}` : "Pooled"}
            </span>
          )}
        </span>
      ),
      actions: (
        <DiskActions
          vm={vm}
          diskId={disk.id}
          disabled={disabled}
          onDetach={(purge, ack) =>
            run(
              () => detachDisk(vm.vmid, disk.id, purge, ack),
              `${disk.id} detached${purge ? " and its volume removed" : ""}`,
            )
          }
        />
      ),
    })),
    // Read-only for now: a drive is defined with the machine, and changing
    // what is in one after the fact needs an eject/insert the API does not
    // have yet. Shown regardless, because a machine that boots off media an
    // operator cannot see is a machine nobody can explain.
    ...vm.cdroms.map((cdrom) => ({
      key: `cdrom-${cdrom.id}`,
      icon: Disc,
      device: `CD/DVD Drive (${cdrom.id})`,
      text: cdrom.source ? (cdrom.source.split("/").pop() ?? cdrom.source) : "empty",
      value: (
        <span className="qz-mono" title={cdrom.source ?? undefined}>
          {cdrom.source ? cdrom.source.split("/").pop() : "empty"}
        </span>
      ),
    })),
    ...vm.nics.map((nic, index) => ({
      key: `nic-${nic.id}`,
      icon: Network,
      device: `Network Device (net${index})`,
      text: nicText(nic),
      value: (
        <span className="qz-mono" title={nicText(nic)}>
          {nicText(nic)}
        </span>
      ),
      actions: (
        <RowActions
          label={`adapter ${nic.id}`}
          onEdit={() => undefined}
          editDisabled
          editTitle="An adapter is replaced rather than edited — remove it and add another."
          deleteDisabled={disabled}
          onDelete={() => run(() => detachNic(vm.vmid, nic.id), `${nic.id} removed`)}
        />
      ),
    })),
  ];

  return (
    <div className="flex flex-col gap-4">
      {vm.pending_reboot.length > 0 && (
        <div className="callout callout-warn">
          <Clock size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="flex-1 min-w-0">
            <div className="text-[13px] font-semibold text-[var(--qz-fg-1)]">
              Waiting for a restart
            </div>
            <ul className="m-0 mt-1 pl-4 text-[13px] text-[var(--qz-fg-3)]">
              {vm.pending_reboot.map((item) => (
                <li key={item}>{item}</li>
              ))}
            </ul>
          </div>
        </div>
      )}

      <DataTable
        rows={rows}
        columns={hardwareColumns}
        rowId={(row) => row.key}
        storageKey="vm-hardware"
        searchPlaceholder="Search hardware…"
        emptyMessage="This machine has no hardware at all, which should not be possible."
        actionsWidth={100}
        toolbar={
          <>
            <Button
              kind="secondary"
              size="sm"
              icon={Plus}
              disabled={disabled}
              onClick={() => setAdding("disk")}
            >
              Add disk
            </Button>
            <Button
              kind="secondary"
              size="sm"
              icon={Plus}
              disabled={disabled}
              onClick={() => setAdding("nic")}
            >
              Add adapter
            </Button>
            {allReplicated && (
              <span
                title={
                  migrateTargets.length === 0
                    ? "No other node holds a replica of every disk."
                    : `Live-migrate to ${migrateTargets.join(" or ")}.`
                }
              >
                <Button
                  kind="secondary"
                  size="sm"
                  icon={ArrowRightLeft}
                  disabled={disabled || migrateTargets.length === 0}
                  onClick={() => setMigrating(true)}
                >
                  Migrate
                </Button>
              </span>
            )}
          </>
        }
        actions={(row) => row.actions ?? null}
      />

      {adding === "disk" && (
        <AddDiskDialog
          vm={vm}
          onClose={() => setAdding(null)}
          onAdded={async (response) => {
            setAdding(null);
            await report(response, "Disk attached");
          }}
        />
      )}
      {adding === "nic" && (
        <AddNicDialog
          vm={vm}
          onClose={() => setAdding(null)}
          onAdded={async (response) => {
            setAdding(null);
            await report(response, "Adapter attached");
          }}
        />
      )}
      {migrating && (
        <MigrateDialog
          vm={vm}
          targets={migrateTargets}
          onClose={() => setMigrating(false)}
          onMigrated={async (target) => {
            setMigrating(false);
            // The machine has one home now, and it is not this node — the
            // reload drops it from this console's list.
            await onChanged(`${vm.name} migrated to ${target}.`);
          }}
        />
      )}
      {editing !== null &&
        (() => {
          const dialogs: Record<
            SizingKey,
            [React.ComponentType<SizingDialogProps>, string]
          > = {
            memory: [EditMemoryDialog, "Memory updated"],
            processors: [EditProcessorsDialog, "Processors updated"],
            bios: [EditFirmwareDialog, "Firmware updated"],
            display: [EditDisplayDialog, "Display updated"],
            machine: [EditMachineDialog, "Machine type updated"],
          };
          const [Dialog, message] = dialogs[editing];
          return (
            <Dialog
              vm={vm}
              onClose={() => setEditing(null)}
              onSaved={async (response) => {
                setEditing(null);
                await report(response, message);
              }}
            />
          );
        })()}
    </div>
  );
}

/// The five rows above the devices, each with a dialog of its own.
type SizingKey = "memory" | "processors" | "bios" | "display" | "machine";

interface SizingDialogProps {
  vm: VmView;
  onClose: () => void;
  onSaved: (response: VmUpdateResponse) => Promise<void>;
}

/// The pencil RowActions draws, without the delete half: a sizing row is not
/// removable, so a pair with a permanently disabled bin would be noise.
function EditPencil({
  label,
  disabled,
  onClick,
}: {
  label: string;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      title={`Edit ${label}`}
      aria-label={`Edit ${label}`}
      disabled={disabled}
      onClick={onClick}
      className="grid place-items-center w-7 h-7 rounded-md bg-transparent border-0 text-[var(--qz-fg-4)] hover:text-[var(--qz-accent)] hover:bg-[color-mix(in_oklab,white_5%,transparent)] transition-colors cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed disabled:hover:text-[var(--qz-fg-4)] disabled:hover:bg-transparent"
    >
      <Pencil size={14} />
    </button>
  );
}

/// Detach, with the purge decision made in the dialog rather than assumed.
function DiskActions({
  vm,
  diskId,
  disabled,
  onDetach,
}: {
  vm: VmView;
  diskId: string;
  disabled: boolean;
  onDetach: (purge: boolean, acknowledge: boolean) => Promise<void>;
}) {
  const [asking, setAsking] = useState(false);
  const [purge, setPurge] = useState(false);
  const [acked, setAcked] = useState(false);
  const [working, setWorking] = useState(false);

  return (
    <>
      <Button kind="ghost" size="sm" disabled={disabled} onClick={() => setAsking(true)}>
        Detach
      </Button>
      {asking && (
        <ModalShell onClose={() => setAsking(false)}>
          <ModalHeader
            title={`Detach ${diskId}?`}
            subtitle={`From ${vm.name} (machine ${vm.vmid}).`}
            onClose={() => setAsking(false)}
          />
          <div className="flex flex-col gap-4">
            <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
              The disk is removed from the machine. Its volume is kept unless you say otherwise —
              detaching is not the same decision as destroying the data on it.
            </p>
            <label className="flex items-center gap-[10px] cursor-pointer select-none">
              <Switch on={purge} onChange={setPurge} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">
                Also destroy the volume behind it
              </span>
            </label>
            {purge && (
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
              onCancel={() => setAsking(false)}
              saving={working}
              disabled={purge && !acked}
              savingLabel="Detaching…"
              submitLabel="Detach"
              onSubmit={async () => {
                setWorking(true);
                try {
                  await onDetach(purge, acked);
                  setAsking(false);
                } finally {
                  setWorking(false);
                }
              }}
            />
          </div>
        </ModalShell>
      )}
    </>
  );
}

/// The migration dialog: one target, one sentence about what happens, one
/// button. The two-primaries window is the backend's guard, not a choice.
function MigrateDialog({
  vm,
  targets,
  onClose,
  onMigrated,
}: {
  vm: VmView;
  targets: string[];
  onClose: () => void;
  onMigrated: (target: string) => Promise<void>;
}) {
  const [target, setTarget] = useState(targets[0] ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const migrate = async () => {
    setBusy(true);
    setError(null);
    try {
      const answer = await migrateVm(vm.vmid, target);
      await onMigrated(answer.target);
    } catch (err) {
      setError(err instanceof Error ? err.message : "The migration failed.");
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title={`Migrate ${vm.name}`}
        subtitle="The machine keeps running while its memory moves over the cluster's Core network. Its disks are already on both sides."
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        {error && <ErrorText msg={error} />}
        <Field
          label="Target"
          htmlFor="migrate-target"
          hint="A pool member. The storage layer permits both sides to hold the disk open only for the moment of the handover, and the window is closed again on every outcome."
        >
          <SelectInput
            id="migrate-target"
            value={target}
            mono
            onChange={setTarget}
          >
            {targets.map((node) => (
              <option key={node} value={node}>
                {node}
              </option>
            ))}
          </SelectInput>
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!target}
          submitLabel="Migrate"
          savingLabel="Migrating…"
          onSubmit={() => void migrate()}
        />
      </div>
    </ModalShell>
  );
}

function AddDiskDialog({
  vm,
  onClose,
  onAdded,
}: {
  vm: VmView;
  onClose: () => void;
  onAdded: (response: VmUpdateResponse) => Promise<void>;
}) {
  const [pools, setPools] = useState<PoolView[] | null>(null);
  const [pool, setPool] = useState("");
  const [sizeGib, setSizeGib] = useState("32");
  const [bus, setBus] = useState<DiskBus>("virtio-blk");
  const [discard, setDiscard] = useState(true);
  const [replicated, setReplicated] = useState(false);
  /// Whether this node's cluster serves a pool — what makes the replicated
  /// switch worth showing at all. The pool places a disk by itself, so the
  /// switch is the whole of the choice.
  const [hasPool, setHasPool] = useState(false);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const response = await fetchPools();
        const found = response.nodes.flatMap((node) => node.pools);
        setPools(found);
        setPool(found[0]?.name ?? "");
      } catch {
        setPools([]);
      }
      try {
        const response = await fetchPooledStorage();
        setHasPool(response.pool !== null);
      } catch {
        /* a standalone node simply has no replicated choice */
      }
    })();
  }, []);

  const submit = async () => {
    const size = numberOf(sizeGib);
    if (size === undefined || size < 1) {
      setErrors({ size_gib: "Use a size of at least 1 GiB." });
      return;
    }
    if (!replicated && !pool) {
      setErrors({ pool: "Choose a pool." });
      return;
    }
    setSaving(true);
    try {
      const body = replicated
        ? { size_gib: size, bus, discard, replicated: true }
        : { pool, size_gib: size, bus, discard };
      await onAdded(await attachDisk(vm.vmid, body));
    } catch (err) {
      const detail = validationErrorsOf(err);
      const found: Record<string, string> = {};
      for (const item of detail) found[item.field ?? "form"] = item.message;
      if (detail.length === 0) {
        found.form = err instanceof Error ? err.message : "Something went wrong.";
      }
      setErrors(found);
    } finally {
      setSaving(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Add a disk"
        subtitle={`A new volume is created for ${vm.name}.`}
        onClose={onClose}
      />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {hasPool && (
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <Switch on={replicated} onChange={setReplicated} />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              Replicated across the cluster — served by the pool on every member at once
            </span>
          </label>
        )}
        {!replicated && (
        <Field label="Pool" htmlFor="disk-pool" required error={errors.pool}>
          <SelectInput
            id="disk-pool"
            value={pool}
            mono
            invalid={!!errors.pool}
            onChange={setPool}
          >
            {(pools ?? []).map((candidate) => (
              <option key={candidate.name} value={candidate.name}>
                {candidate.name} — {formatBytes(candidate.free)} free
              </option>
            ))}
            {pools !== null && pools.length === 0 && <option value="">No pools on this node</option>}
          </SelectInput>
        </Field>
        )}
        <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
          <Field label="Size (GiB)" htmlFor="disk-size" required error={errors.size_gib}>
            <TextInput
              id="disk-size"
              value={sizeGib}
              mono
              inputMode="numeric"
              invalid={!!errors.size_gib}
              onChange={setSizeGib}
            />
          </Field>
          <Field label="Bus" htmlFor="disk-bus">
            <SelectInput id="disk-bus" value={bus} mono onChange={(v) => setBus(v as DiskBus)}>
              <option value="virtio-blk">virtio-blk</option>
              <option value="virtio-scsi">virtio-scsi</option>
              <option value="sata">sata</option>
            </SelectInput>
          </Field>
        </div>
        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <Switch on={discard} onChange={setDiscard} />
          <span className="text-[13px] text-[var(--qz-fg-2)]">Pass discard through to the pool</span>
        </label>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          savingLabel="Creating…"
          submitLabel="Add disk"
        />
      </form>
    </ModalShell>
  );
}

function AddNicDialog({
  vm,
  onClose,
  onAdded,
}: {
  vm: VmView;
  onClose: () => void;
  onAdded: (response: VmUpdateResponse) => Promise<void>;
}) {
  const [bridges, setBridges] = useState<string[] | null>(null);
  const [bridge, setBridge] = useState("");
  const [model, setModel] = useState<NicModel>("virtio");
  const [vlanTag, setVlanTag] = useState("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const response = await fetchInterfaces();
        const found = response.nodes
          .flatMap((node) => node.interfaces)
          .filter((link) => link.kind === "bridge")
          .map((link) => link.name);
        setBridges(found);
        setBridge(found[0] ?? "");
      } catch {
        setBridges([]);
      }
    })();
  }, []);

  const submit = async () => {
    if (!bridge) {
      setErrors({ bridge: "Choose a bridge." });
      return;
    }
    const tag = numberOf(vlanTag);
    if (vlanTag.trim() !== "" && (tag === undefined || tag < 1 || tag > 4094)) {
      setErrors({ vlan_tag: "Use a tag from 1 to 4094." });
      return;
    }
    setSaving(true);
    try {
      await onAdded(await attachNic(vm.vmid, { bridge, model, vlan_tag: tag }));
    } catch (err) {
      const detail = validationErrorsOf(err);
      const found: Record<string, string> = {};
      for (const item of detail) found[item.field ?? "form"] = item.message;
      if (detail.length === 0) {
        found.form = err instanceof Error ? err.message : "Something went wrong.";
      }
      setErrors(found);
    } finally {
      setSaving(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Add a network adapter"
        subtitle={`Attached to a bridge on ${vm.node}.`}
        onClose={onClose}
      />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <Field label="Bridge" htmlFor="nic-bridge" required error={errors.bridge}>
          <SelectInput
            id="nic-bridge"
            value={bridge}
            mono
            invalid={!!errors.bridge}
            onChange={setBridge}
          >
            {(bridges ?? []).map((candidate) => (
              <option key={candidate} value={candidate}>
                {candidate}
              </option>
            ))}
            {bridges !== null && bridges.length === 0 && (
              <option value="">No bridges on this node</option>
            )}
          </SelectInput>
        </Field>
        <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
          <Field label="Model" htmlFor="nic-model">
            <SelectInput
              id="nic-model"
              value={model}
              mono
              onChange={(v) => setModel(v as NicModel)}
            >
              <option value="virtio">virtio</option>
              <option value="e1000e">e1000e</option>
              <option value="rtl8139">rtl8139</option>
            </SelectInput>
          </Field>
          <Field label="VLAN tag" htmlFor="nic-vlan" error={errors.vlan_tag}>
            <TextInput
              id="nic-vlan"
              value={vlanTag}
              mono
              inputMode="numeric"
              invalid={!!errors.vlan_tag}
              placeholder="none"
              onChange={setVlanTag}
            />
          </Field>
        </div>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          savingLabel="Attaching…"
          submitLabel="Add adapter"
        />
      </form>
    </ModalShell>
  );
}

/// The submit plumbing every sizing dialog shares: send the patch, pin the
/// node's validation sentences to their fields, and never invent a message the
/// backend already wrote.
function usePatchForm(vm: VmView, onSaved: (response: VmUpdateResponse) => Promise<void>) {
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  const save = async (patch: VmPatch) => {
    setSaving(true);
    try {
      await onSaved(await updateVm(vm.vmid, patch));
    } catch (err) {
      const detail = validationErrorsOf(err);
      const pinned: Record<string, string> = {};
      for (const item of detail) pinned[item.field ?? "form"] = item.message;
      if (detail.length === 0) {
        pinned.form = err instanceof Error ? err.message : "Something went wrong.";
      }
      setErrors(pinned);
    } finally {
      setSaving(false);
    }
  };

  return { errors, setErrors, saving, save };
}

/// The one subtitle every sizing dialog wants: whether the change lands now or
/// at the next start is the node's answer, not this dialog's prediction.
const appliesWhen = (vm: VmView) =>
  vm.state === "running"
    ? "The node will say whether the running machine takes this now or waits for a restart."
    : "The machine is stopped, so this takes effect the next time it starts.";

function EditMemoryDialog({ vm, onClose, onSaved }: SizingDialogProps) {
  const [memoryMib, setMemoryMib] = useState(String(vm.memory_mib));
  const { errors, setErrors, saving, save } = usePatchForm(vm, onSaved);

  const parsed = numberOf(memoryMib);

  const submit = async () => {
    if (parsed === undefined || parsed < 128) {
      setErrors({ memory_mib: "Use at least 128 MiB." });
      return;
    }
    await save({ memory_mib: parsed });
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader title="Memory" subtitle={appliesWhen(vm)} onClose={onClose} />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <Field
          label="Memory (MiB)"
          htmlFor="hw-memory"
          required
          error={errors.memory_mib}
          hint={parsed !== undefined && parsed >= 128 ? formatMib(parsed) : undefined}
        >
          <TextInput
            id="hw-memory"
            value={memoryMib}
            mono
            inputMode="numeric"
            invalid={!!errors.memory_mib}
            onChange={setMemoryMib}
          />
        </Field>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

/// Sockets × cores, the way the wizard asks: the node refuses a layout that
/// multiplies out to a different total, so asking for the total and the layout
/// separately is asking to be refused.
function EditProcessorsDialog({ vm, onClose, onSaved }: SizingDialogProps) {
  const [sockets, setSockets] = useState(String(vm.topology?.sockets ?? 1));
  const [cores, setCores] = useState(String(vm.topology?.cores ?? vm.vcpus));
  const [cpuType, setCpuType] = useState<string>(
    typeof vm.cpu_model === "string" ? vm.cpu_model : vm.cpu_model.named,
  );
  const [cpus, setCpus] = useState<CpuModels | null>(null);
  const { errors, setErrors, saving, save } = usePatchForm(vm, onSaved);

  useEffect(() => {
    void (async () => {
      try {
        setCpus(await fetchCpuModels());
      } catch {
        setCpus({ host_passthrough: true, models: [], reason: "unreadable" });
      }
    })();
  }, []);

  const socketCount = numberOf(sockets);
  const coreCount = numberOf(cores);
  const total =
    socketCount !== undefined && coreCount !== undefined ? socketCount * coreCount : undefined;

  // The machine's current model may be one the list does not carry — the list
  // being unreadable, or a model this node cannot run — and a select that
  // silently swapped it for the default would be an edit nobody made.
  const named = cpus?.models.map((model) => model.name) ?? [];
  const keepCurrent =
    cpuType !== "host_model" && cpuType !== "host_passthrough" && !named.includes(cpuType);

  const submit = async () => {
    if (socketCount === undefined || socketCount < 1 || coreCount === undefined || coreCount < 1) {
      setErrors({ topology: "A machine needs at least one socket and one core." });
      return;
    }
    const model: CpuModel =
      cpuType === "host_model" || cpuType === "host_passthrough" ? cpuType : { named: cpuType };
    await save({
      vcpus: socketCount * coreCount,
      topology: { sockets: socketCount, cores: coreCount, threads: 1 },
      cpu_model: model,
    });
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader title="Processors" subtitle={appliesWhen(vm)} onClose={onClose} />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
          <Field label="Sockets" htmlFor="hw-sockets" required error={errors.topology}>
            <TextInput
              id="hw-sockets"
              value={sockets}
              mono
              inputMode="numeric"
              invalid={!!errors.topology}
              onChange={setSockets}
            />
          </Field>
          <Field
            label="Cores"
            htmlFor="hw-cores"
            required
            error={errors.vcpus}
            hint={total !== undefined ? `${total} processor${total === 1 ? "" : "s"} in total.` : undefined}
          >
            <TextInput
              id="hw-cores"
              value={cores}
              mono
              inputMode="numeric"
              invalid={!!errors.vcpus}
              onChange={setCores}
            />
          </Field>
        </div>
        <Field label="Type" htmlFor="hw-cpu-type" error={errors.cpu_model}>
          <SelectInput id="hw-cpu-type" value={cpuType} onChange={setCpuType}>
            <option value="host_model">
              Default (host model{cpus?.host_model ? ` — ${cpus.host_model}` : ""})
            </option>
            {(cpus?.host_passthrough ?? true) && (
              <option value="host_passthrough">Host passthrough</option>
            )}
            {keepCurrent && <option value={cpuType}>{cpuType}</option>}
            {cpus?.models.map((model) => (
              <option key={model.name} value={model.name} disabled={!model.usable}>
                {model.name}
                {model.usable ? "" : " — not runnable on this node"}
              </option>
            ))}
          </SelectInput>
        </Field>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

function EditFirmwareDialog({ vm, onClose, onSaved }: SizingDialogProps) {
  const [firmware, setFirmware] = useState(vm.firmware);
  const { errors, saving, save } = usePatchForm(vm, onSaved);

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader title="Firmware" subtitle={appliesWhen(vm)} onClose={onClose} />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save({ firmware });
        }}
      >
        <Field
          label="Firmware"
          htmlFor="hw-firmware"
          error={errors.firmware}
          hint="A guest boots the way it was installed — an operating system put down under one firmware does not usually start under the other."
        >
          <SelectInput
            id="hw-firmware"
            value={firmware}
            onChange={(v) => setFirmware(v as typeof firmware)}
          >
            <option value="uefi">UEFI</option>
            <option value="bios">Legacy BIOS</option>
          </SelectInput>
        </Field>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

function EditDisplayDialog({ vm, onClose, onSaved }: SizingDialogProps) {
  const [video, setVideo] = useState(vm.video);
  const { errors, saving, save } = usePatchForm(vm, onSaved);

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Display"
        subtitle={
          vm.has_screen
            ? appliesWhen(vm)
            : "This machine has no display device yet. Saving gives it one; it appears after the machine is stopped and started."
        }
        onClose={onClose}
      />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save({ video });
        }}
      >
        <Field
          label="Graphics card"
          htmlFor="hw-video"
          hint={VIDEO_HINT[video]}
          error={errors.video}
        >
          <SelectInput id="hw-video" value={video} onChange={(v) => setVideo(v as VideoModel)}>
            {(Object.keys(VIDEO_LABEL) as VideoModel[]).map((model) => (
              <option key={model} value={model}>
                {VIDEO_LABEL[model]}
              </option>
            ))}
          </SelectInput>
        </Field>
        {/* The same sentence the wizard says, for the same reason: legal, and
            a black console rather than an error. */}
        {video === "bochs" && vm.firmware === "bios" && (
          <div className="callout callout-warn">
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              Bochs has no VGA BIOS underneath it, so on legacy BIOS this machine will show nothing
              on the console until the guest&apos;s own driver loads. Standard VGA draws from the
              first frame.
            </span>
          </div>
        )}
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

function EditMachineDialog({ vm, onClose, onSaved }: SizingDialogProps) {
  // The stored type is the hypervisor's canonical name — "pc-q35-rhel10.2.0"
  // — and the choice on offer is the chipset, so the name is read back down
  // to the one word that picks it. Saving sends the word; the hypervisor
  // re-canonicalizes it.
  const [machine, setMachine] = useState(vm.machine.includes("q35") ? "q35" : "pc");
  const { errors, saving, save } = usePatchForm(vm, onSaved);

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Machine type"
        subtitle={`Currently ${vm.machine}. ${appliesWhen(vm)}`}
        onClose={onClose}
      />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void save({ machine });
        }}
      >
        <Field
          label="Machine type"
          htmlFor="hw-machine"
          error={errors.machine}
          hint="The chipset the guest sees. q35 is PCIe and the one to keep; i440fx is for guests too old to know what that is. Changing it re-plumbs every bus, so an installed guest may need reconfiguring."
        >
          <SelectInput id="hw-machine" value={machine} onChange={setMachine}>
            <option value="q35">q35</option>
            <option value="pc">i440fx</option>
          </SelectInput>
        </Field>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

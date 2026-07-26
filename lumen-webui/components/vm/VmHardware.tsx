"use client";

import { useEffect, useState } from "react";
import {
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
  Plus,
  type LucideIcon,
} from "lucide-react";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ErrorText, Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { DataTable, type Column } from "@/components/console/DataTable";
import { RowActions } from "@/components/console/RowActions";
import { fetchInterfaces } from "@/lib/networkClient";
import { fetchPools, type PoolView } from "@/lib/storageClient";
import {
  attachDisk,
  attachNic,
  cpuModelLabel,
  detachDisk,
  detachNic,
  formatBytes,
  formatMib,
  updateVm,
  validationErrorsOf,
  VIDEO_HINT,
  VIDEO_LABEL,
  type BootDevice,
  type DiskBus,
  type NicModel,
  type VideoModel,
  type VmUpdateResponse,
  type VmView,
} from "@/lib/vmClient";

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
  const [editingSizing, setEditingSizing] = useState(false);
  const [working, setWorking] = useState(false);

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

  // The one dialog behind every sizing row — memory, processors, BIOS,
  // display, and machine type are edited together, and the node answers which
  // changes the running machine took.
  const sizingEdit = (
    <Button kind="ghost" size="sm" disabled={disabled} onClick={() => setEditingSizing(true)}>
      Edit
    </Button>
  );

  const processors = `${vm.vcpus}${
    vm.topology ? ` (${vm.topology.sockets} sockets, ${vm.topology.cores} cores)` : ""
  } [${cpuModelLabel(vm.cpu_model)}]`;

  const diskText = (disk: (typeof vm.disks)[number]) =>
    `${disk.source}, ${disk.bus}, ${formatBytes(disk.size)}` +
    `${disk.cache !== "none" ? `, cache=${disk.cache}` : ""}` +
    `${disk.discard ? ", discard" : ""}` +
    `${disk.boot_index != null ? `, boot=${disk.boot_index}` : ""}`;

  const nicText = (nic: (typeof vm.nics)[number]) =>
    `${nic.model}=${nic.id},bridge=${nic.bridge}${nic.vlan_tag != null ? `,tag=${nic.vlan_tag}` : ""}`;

  const rows: HardwareRow[] = [
    {
      key: "memory",
      icon: MemoryStick,
      device: "Memory",
      text: formatMib(vm.memory_mib),
      value: <span className="qz-mono">{formatMib(vm.memory_mib)}</span>,
      actions: sizingEdit,
    },
    {
      key: "processors",
      icon: Cpu,
      device: "Processors",
      text: processors,
      value: <span className="qz-mono">{processors}</span>,
      actions: sizingEdit,
    },
    {
      key: "bios",
      icon: CircuitBoard,
      device: "BIOS",
      text: vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS",
      value: vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS",
      actions: sizingEdit,
    },
    {
      key: "display",
      icon: Monitor,
      device: "Display",
      /* Not simply the card. A machine whose document has no display device
         reads back as the default one, so printing `video` here told an
         operator "VirtIO GPU" about a machine with no graphics device and no
         console socket — and then the console said there was no screen, which
         reads as a broken appliance rather than as a machine that needs
         saving. */
      text: vm.has_screen ? VIDEO_LABEL[vm.video] : "none yet",
      value: vm.has_screen ? (
        VIDEO_LABEL[vm.video]
      ) : (
        <span style={{ color: "var(--qz-warn)" }}>
          none yet <span className="qz-dim">— save to give it {VIDEO_LABEL[vm.video]}</span>
        </span>
      ),
      actions: sizingEdit,
    },
    {
      key: "machine",
      icon: Box,
      device: "Machine",
      text: vm.machine,
      value: <span className="qz-mono">{vm.machine}</span>,
      actions: sizingEdit,
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
        <span className="qz-mono" title={diskText(disk)}>
          {diskText(disk)}
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
      {editingSizing && (
        <SizingDialog
          vm={vm}
          onClose={() => setEditingSizing(false)}
          onSaved={async (response) => {
            setEditingSizing(false);
            await report(response, "Hardware updated");
          }}
        />
      )}
    </div>
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
    })();
  }, []);

  const submit = async () => {
    const size = numberOf(sizeGib);
    if (!pool) {
      setErrors({ pool: "Choose a pool." });
      return;
    }
    if (size === undefined || size < 1) {
      setErrors({ size_gib: "Use a size of at least 1 GiB." });
      return;
    }
    setSaving(true);
    try {
      await onAdded(await attachDisk(vm.vmid, { pool, size_gib: size, bus, discard }));
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

/// Processors, memory, firmware, and boot order. Everything here except the
/// first two needs a restart on a running machine, and the answer comes back
/// from the node rather than being predicted in this dialog.
function SizingDialog({
  vm,
  onClose,
  onSaved,
}: {
  vm: VmView;
  onClose: () => void;
  onSaved: (response: VmUpdateResponse) => Promise<void>;
}) {
  const [vcpus, setVcpus] = useState(String(vm.vcpus));
  const [memoryMib, setMemoryMib] = useState(String(vm.memory_mib));
  const [firmware, setFirmware] = useState(vm.firmware);
  const [video, setVideo] = useState(vm.video);
  const [bootOrder, setBootOrder] = useState<string>(vm.boot_order.join(","));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  const submit = async () => {
    const found: Record<string, string> = {};
    const cpus = numberOf(vcpus);
    if (cpus === undefined || cpus < 1) found.vcpus = "A machine needs at least one processor.";
    const memory = numberOf(memoryMib);
    if (memory === undefined || memory < 128) found.memory_mib = "Use at least 128 MiB.";
    setErrors(found);
    if (Object.keys(found).length > 0) return;

    setSaving(true);
    try {
      await onSaved(
        await updateVm(vm.vmid, {
          vcpus: cpus,
          memory_mib: memory,
          firmware,
          video,
          boot_order: bootOrder
            .split(",")
            .map((entry) => entry.trim())
            .filter(Boolean) as BootDevice[],
        }),
      );
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

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Hardware for ${vm.name}`}
        subtitle={
          vm.state === "running"
            ? "The node will say which of these it could apply now and which wait for a restart."
            : "The machine is stopped, so everything takes effect the next time it starts."
        }
        onClose={onClose}
      />
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
          <Field label="Processors" htmlFor="hw-vcpus" required error={errors.vcpus}>
            <TextInput
              id="hw-vcpus"
              value={vcpus}
              mono
              inputMode="numeric"
              invalid={!!errors.vcpus}
              onChange={setVcpus}
            />
          </Field>
          <Field label="Memory (MiB)" htmlFor="hw-memory" required error={errors.memory_mib}>
            <TextInput
              id="hw-memory"
              value={memoryMib}
              mono
              inputMode="numeric"
              invalid={!!errors.memory_mib}
              onChange={setMemoryMib}
            />
          </Field>
        </div>
        <Field label="Firmware" htmlFor="hw-firmware" error={errors.firmware}>
          <SelectInput
            id="hw-firmware"
            value={firmware}
            onChange={(v) => setFirmware(v as typeof firmware)}
          >
            <option value="uefi">UEFI</option>
            <option value="bios">Legacy BIOS</option>
          </SelectInput>
        </Field>
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
        <Field
          label="Boot order"
          htmlFor="hw-boot"
          hint="Comma separated: disk, network. A device the machine does not have is ignored."
          error={errors.boot_order}
        >
          <TextInput
            id="hw-boot"
            value={bootOrder}
            mono
            placeholder="disk,network"
            onChange={setBootOrder}
          />
        </Field>
        {errors.form && <ErrorText msg={errors.form} />}
        <ModalFooter onCancel={onClose} saving={saving} savingLabel="Saving…" submitLabel="Save" />
      </form>
    </ModalShell>
  );
}

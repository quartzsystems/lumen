"use client";

import { useEffect, useState } from "react";
import { Clock, Plus } from "lucide-react";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { ErrorText, Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { RowActions } from "@/components/console/RowActions";
import { Fact, Facts, Mono, Panel } from "@/components/vm/VmBits";
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

/// Disks, adapters, and the sizing that only changes when the machine
/// restarts.
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

      <Panel
        title="Disks"
        actions={
          <Button
            kind="secondary"
            size="sm"
            icon={Plus}
            disabled={disabled}
            onClick={() => setAdding("disk")}
          >
            Add disk
          </Button>
        }
      >
        {vm.disks.length === 0 ? (
          <p className="text-[13px] text-[var(--qz-fg-4)] m-0">No disks.</p>
        ) : (
          <div className="rounded-md overflow-x-auto" style={{ border: "1px solid var(--qz-border)" }}>
            <table className="qz-table">
              <thead>
                <tr>
                  <th>Target</th>
                  <th>Bus</th>
                  <th>Volume</th>
                  <th>Size</th>
                  <th>Cache</th>
                  <th>Boot</th>
                  <th className="text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {vm.disks.map((disk) => (
                  <tr key={disk.id} style={{ cursor: "default" }}>
                    <td className="mono">{disk.id}</td>
                    <td className="mono">{disk.bus}</td>
                    <td className="mono" title={disk.source}>
                      {disk.source}
                    </td>
                    <td>{formatBytes(disk.size)}</td>
                    <td className="mono">{disk.cache}</td>
                    <td className="mono">{disk.boot_index ?? "—"}</td>
                    <td className="text-right">
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
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      {/* Read-only for now: a drive is defined with the machine, and changing
          what is in one after the fact needs an eject/insert the API does not
          have yet. Shown regardless, because a machine that boots off media an
          operator cannot see is a machine nobody can explain. */}
      {vm.cdroms.length > 0 && (
        <Panel title="Optical drives">
          <div className="rounded-md overflow-x-auto" style={{ border: "1px solid var(--qz-border)" }}>
            <table className="qz-table">
              <thead>
                <tr>
                  <th>Target</th>
                  <th>Image</th>
                  <th>Boot</th>
                </tr>
              </thead>
              <tbody>
                {vm.cdroms.map((cdrom) => (
                  <tr key={cdrom.id} style={{ cursor: "default" }}>
                    <td className="mono">{cdrom.id}</td>
                    <td className="mono" title={cdrom.source ?? undefined}>
                      {cdrom.source ? cdrom.source.split("/").pop() : "empty"}
                    </td>
                    <td className="mono">{cdrom.boot_index ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </Panel>
      )}

      <Panel
        title="Network adapters"
        actions={
          <Button
            kind="secondary"
            size="sm"
            icon={Plus}
            disabled={disabled}
            onClick={() => setAdding("nic")}
          >
            Add adapter
          </Button>
        }
      >
        {vm.nics.length === 0 ? (
          <p className="text-[13px] text-[var(--qz-fg-4)] m-0">No adapters.</p>
        ) : (
          <div className="rounded-md overflow-x-auto" style={{ border: "1px solid var(--qz-border)" }}>
            <table className="qz-table">
              <thead>
                <tr>
                  <th>Hardware address</th>
                  <th>Bridge</th>
                  <th>Model</th>
                  <th>VLAN</th>
                  <th className="text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {vm.nics.map((nic) => (
                  <tr key={nic.id} style={{ cursor: "default" }}>
                    <td className="mono">{nic.id}</td>
                    <td className="mono">{nic.bridge}</td>
                    <td className="mono">{nic.model}</td>
                    <td className="mono">{nic.vlan_tag ?? "—"}</td>
                    <td className="text-right">
                      <RowActions
                        label={`adapter ${nic.id}`}
                        onEdit={() => undefined}
                        editDisabled
                        editTitle="An adapter is replaced rather than edited — remove it and add another."
                        deleteDisabled={disabled}
                        onDelete={() =>
                          run(() => detachNic(vm.vmid, nic.id), `${nic.id} removed`)
                        }
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      <Panel
        title="Processor, memory, and boot"
        actions={
          <Button
            kind="secondary"
            size="sm"
            disabled={disabled}
            onClick={() => setEditingSizing(true)}
          >
            Edit
          </Button>
        }
      >
        <Facts>
          <Fact label="Processors">{vm.vcpus}</Fact>
          <Fact label="Memory">{formatMib(vm.memory_mib)}</Fact>
          <Fact label="Processor model">{cpuModelLabel(vm.cpu_model)}</Fact>
          <Fact label="Processor layout">
            {vm.topology
              ? `${vm.topology.sockets} × ${vm.topology.cores} × ${vm.topology.threads}`
              : "left to the hypervisor"}
          </Fact>
          <Fact label="Firmware">{vm.firmware === "uefi" ? "UEFI" : "Legacy BIOS"}</Fact>
          <Fact label="Machine type">
            <Mono>{vm.machine}</Mono>
          </Fact>
          <Fact label="Graphics">{VIDEO_LABEL[vm.video]}</Fact>
          <Fact label="Boot order">
            <Mono>{vm.boot_order.length > 0 ? vm.boot_order.join(" → ") : "—"}</Mono>
          </Fact>
        </Facts>
      </Panel>

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

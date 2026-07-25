"use client";

import { useEffect, useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { Switch } from "@/components/ui/Switch";
import { ErrorText, Field, SelectInput, TextInput } from "@/components/ui/formkit";
import { Button } from "@/components/ui/Button";
import { fetchInterfaces } from "@/lib/networkClient";
import { fetchPools, type PoolView } from "@/lib/storageClient";
import {
  createVm,
  formatBytes,
  validationErrorsOf,
  type CpuModel,
  type DiskBus,
  type Firmware,
  type NicModel,
  type VmView,
} from "@/lib/vmClient";

type Step = "general" | "system" | "disk" | "network" | "review";

const STEPS: { id: Step; label: string }[] = [
  { id: "general", label: "General" },
  { id: "system", label: "System" },
  { id: "disk", label: "Disk" },
  { id: "network", label: "Network" },
  { id: "review", label: "Review" },
];

/// Every field the wizard collects. Numbers are kept as strings so a
/// half-typed one is a normal input state rather than NaN — the same rule
/// `LinkDialog` follows.
interface Draft {
  name: string;
  description: string;
  tags: string;
  vcpus: string;
  memoryMib: string;
  cpuModel: "host_model" | "host_passthrough";
  firmware: Firmware;
  guestAgent: boolean;
  startOnBoot: boolean;
  pool: string;
  sizeGib: string;
  bus: DiskBus;
  discard: boolean;
  bridge: string;
  nicModel: NicModel;
  vlanTag: string;
  startNow: boolean;
}

const emptyDraft = (): Draft => ({
  name: "",
  description: "",
  tags: "",
  vcpus: "2",
  memoryMib: "4096",
  cpuModel: "host_model",
  firmware: "uefi",
  guestAgent: true,
  startOnBoot: false,
  pool: "",
  sizeGib: "32",
  bus: "virtio-blk",
  discard: true,
  bridge: "",
  nicModel: "virtio",
  vlanTag: "",
  startNow: false,
});

const numberOf = (text: string): number | undefined => {
  const trimmed = text.trim();
  if (trimmed === "") return undefined;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : undefined;
};

/// Create a machine: general, system, disk, network, review.
///
/// A wizard rather than one long form because the four groups are decisions an
/// operator makes in that order, and because the disk and adapter steps have
/// to wait for the node's pools and bridges to load — which they can do while
/// the first two steps are being filled in.
export function CreateVmDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (vm: VmView) => void;
}) {
  const [step, setStep] = useState<Step>("general");
  const [draft, setDraft] = useState<Draft>(emptyDraft);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);
  const [pools, setPools] = useState<PoolView[] | null>(null);
  const [bridges, setBridges] = useState<string[] | null>(null);

  const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((d) => ({ ...d, [key]: value }));

  // The node's pools and bridges. Both are offered as what the node actually
  // has rather than as free text: a machine pointed at a pool or a bridge that
  // is not there defines cleanly and then cannot start.
  useEffect(() => {
    void (async () => {
      try {
        const response = await fetchPools();
        const found = response.nodes.flatMap((node) => node.pools);
        setPools(found);
        setDraft((d) => ({ ...d, pool: d.pool || (found[0]?.name ?? "") }));
      } catch {
        setPools([]);
      }
      try {
        const response = await fetchInterfaces();
        const found = response.nodes
          .flatMap((node) => node.interfaces)
          .filter((link) => link.kind === "bridge")
          .map((link) => link.name);
        setBridges(found);
        setDraft((d) => ({ ...d, bridge: d.bridge || (found[0] ?? "") }));
      } catch {
        setBridges([]);
      }
    })();
  }, []);

  const selectedPool = useMemo(
    () => pools?.find((pool) => pool.name === draft.pool) ?? null,
    [pools, draft.pool],
  );

  const validateStep = (which: Step): boolean => {
    const found: Record<string, string> = {};
    if (which === "general") {
      if (!/^[A-Za-z0-9._-]{1,63}$/.test(draft.name) || draft.name.startsWith("-")) {
        found.name = "Use up to 63 letters, digits, dots, dashes, or underscores.";
      }
    }
    if (which === "system") {
      const vcpus = numberOf(draft.vcpus);
      if (vcpus === undefined || vcpus < 1) found.vcpus = "A machine needs at least one processor.";
      const memory = numberOf(draft.memoryMib);
      if (memory === undefined || memory < 128) found.memory_mib = "Use at least 128 MiB.";
    }
    if (which === "disk" && draft.pool) {
      const size = numberOf(draft.sizeGib);
      if (size === undefined || size < 1) found.size_gib = "Use a size of at least 1 GiB.";
      else if (selectedPool && size * 1024 ** 3 > selectedPool.free) {
        found.size_gib = `"${selectedPool.name}" has ${formatBytes(selectedPool.free)} free.`;
      }
    }
    if (which === "network" && draft.vlanTag.trim() !== "") {
      const tag = numberOf(draft.vlanTag);
      if (tag === undefined || tag < 1 || tag > 4094) found.vlan_tag = "Use a tag from 1 to 4094.";
    }
    setErrors(found);
    return Object.keys(found).length === 0;
  };

  const index = STEPS.findIndex((s) => s.id === step);
  const next = () => {
    if (!validateStep(step)) return;
    setStep(STEPS[Math.min(index + 1, STEPS.length - 1)].id);
  };
  const back = () => {
    setErrors({});
    setStep(STEPS[Math.max(index - 1, 0)].id);
  };

  const submit = async () => {
    // Re-check every step, not just the last: an operator can walk back and
    // break something they already passed.
    for (const candidate of STEPS) {
      if (!validateStep(candidate.id)) {
        setStep(candidate.id);
        return;
      }
    }
    setSaving(true);
    try {
      const cpuModel: CpuModel = draft.cpuModel;
      const vm = await createVm({
        name: draft.name.trim(),
        description: draft.description.trim() || undefined,
        tags: draft.tags
          .split(",")
          .map((tag) => tag.trim())
          .filter(Boolean),
        vcpus: numberOf(draft.vcpus),
        memory_mib: numberOf(draft.memoryMib),
        cpu_model: cpuModel,
        firmware: draft.firmware,
        guest_agent: draft.guestAgent,
        start_on_boot: draft.startOnBoot,
        disks: draft.pool
          ? [
              {
                pool: draft.pool,
                size_gib: numberOf(draft.sizeGib) ?? 0,
                bus: draft.bus,
                discard: draft.discard,
              },
            ]
          : [],
        nics: draft.bridge
          ? [
              {
                bridge: draft.bridge,
                model: draft.nicModel,
                vlan_tag: numberOf(draft.vlanTag),
              },
            ]
          : [],
        start: draft.startNow,
      });
      onCreated(vm);
    } catch (err) {
      const detail = validationErrorsOf(err);
      if (detail.length > 0) {
        // Pin each rejection to the input that caused it, and jump back to the
        // step that input is on.
        const found: Record<string, string> = {};
        for (const item of detail) found[item.field ?? "form"] = item.message;
        setErrors(found);
        const stepOf: Record<string, Step> = {
          name: "general",
          vmid: "general",
          tags: "general",
          vcpus: "system",
          memory_mib: "system",
          topology: "system",
          pool: "disk",
          size_gib: "disk",
          bridge: "network",
          vlan_tag: "network",
        };
        const first = detail.find((item) => item.field && stepOf[item.field]);
        if (first?.field) setStep(stepOf[first.field]);
      } else {
        setErrors({ form: err instanceof Error ? err.message : "Something went wrong." });
      }
    } finally {
      setSaving(false);
    }
  };

  return (
    <ModalShell onClose={onClose} maxWidth={580}>
      <ModalHeader
        title="Create virtual machine"
        subtitle="The machine is defined on this node as soon as you finish."
        onClose={onClose}
      />

      {/* Where we are, and what is left. */}
      <div className="flex items-center gap-[6px] mb-5 flex-wrap">
        {STEPS.map((s, i) => (
          <span
            key={s.id}
            className={`badge ${i === index ? "badge-ok" : i < index ? "badge-info" : "badge-muted"}`}
          >
            {s.label}
          </span>
        ))}
      </div>

      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (step === "review") void submit();
          else next();
        }}
      >
        {step === "general" && (
          <>
            <Field label="Name" htmlFor="vm-name" required error={errors.name}>
              <TextInput
                id="vm-name"
                value={draft.name}
                mono
                autoFocus
                invalid={!!errors.name}
                placeholder="web01"
                onChange={(v) => set("name", v)}
              />
            </Field>
            <Field
              label="Description"
              htmlFor="vm-description"
              hint="Shown on the machine's overview."
            >
              <TextInput
                id="vm-description"
                value={draft.description}
                placeholder="What this machine is for"
                onChange={(v) => set("description", v)}
              />
            </Field>
            <Field label="Tags" htmlFor="vm-tags" hint="Comma separated." error={errors.tags}>
              <TextInput
                id="vm-tags"
                value={draft.tags}
                mono
                invalid={!!errors.tags}
                placeholder="production, web"
                onChange={(v) => set("tags", v)}
              />
            </Field>
            <label className="flex items-center gap-[10px] cursor-pointer select-none">
              <Switch on={draft.startOnBoot} onChange={(v) => set("startOnBoot", v)} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">Start on boot</span>
            </label>
          </>
        )}

        {step === "system" && (
          <>
            <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
              <Field label="Processors" htmlFor="vm-vcpus" required error={errors.vcpus}>
                <TextInput
                  id="vm-vcpus"
                  value={draft.vcpus}
                  mono
                  inputMode="numeric"
                  invalid={!!errors.vcpus}
                  onChange={(v) => set("vcpus", v)}
                />
              </Field>
              <Field label="Memory (MiB)" htmlFor="vm-memory" required error={errors.memory_mib}>
                <TextInput
                  id="vm-memory"
                  value={draft.memoryMib}
                  mono
                  inputMode="numeric"
                  invalid={!!errors.memory_mib}
                  onChange={(v) => set("memoryMib", v)}
                />
              </Field>
            </div>
            <Field
              label="Processor model"
              htmlFor="vm-cpu"
              hint="Host model keeps the machine movable to a similar node; passthrough does not."
            >
              <SelectInput
                id="vm-cpu"
                value={draft.cpuModel}
                onChange={(v) => set("cpuModel", v as Draft["cpuModel"])}
              >
                <option value="host_model">Host model</option>
                <option value="host_passthrough">Host passthrough</option>
              </SelectInput>
            </Field>
            <Field
              label="Firmware"
              htmlFor="vm-firmware"
              hint="Chosen once, at creation — a machine cannot change firmware later."
            >
              <SelectInput
                id="vm-firmware"
                value={draft.firmware}
                onChange={(v) => set("firmware", v as Firmware)}
              >
                <option value="uefi">UEFI</option>
                <option value="bios">Legacy BIOS</option>
              </SelectInput>
            </Field>
            <label className="flex items-center gap-[10px] cursor-pointer select-none">
              <Switch on={draft.guestAgent} onChange={(v) => set("guestAgent", v)} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">Guest agent channel</span>
            </label>
          </>
        )}

        {step === "disk" && (
          <>
            {pools === null ? (
              <p className="text-[13px] text-[var(--qz-fg-4)] m-0">Reading the node's pools…</p>
            ) : pools.length === 0 ? (
              <div className="callout callout-warn">
                <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
                <div className="text-[13px] text-[var(--qz-fg-2)]">
                  This node has no storage pools, so the machine will be created without a disk. Add
                  one later from its Hardware page.
                </div>
              </div>
            ) : (
              <>
                <Field label="Pool" htmlFor="vm-pool" error={errors.pool}>
                  <SelectInput
                    id="vm-pool"
                    value={draft.pool}
                    mono
                    invalid={!!errors.pool}
                    onChange={(v) => set("pool", v)}
                  >
                    {pools.map((pool) => (
                      <option key={pool.name} value={pool.name}>
                        {pool.name} — {formatBytes(pool.free)} free
                      </option>
                    ))}
                  </SelectInput>
                </Field>
                <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
                  <Field label="Size (GiB)" htmlFor="vm-size" required error={errors.size_gib}>
                    <TextInput
                      id="vm-size"
                      value={draft.sizeGib}
                      mono
                      inputMode="numeric"
                      invalid={!!errors.size_gib}
                      onChange={(v) => set("sizeGib", v)}
                    />
                  </Field>
                  <Field label="Bus" htmlFor="vm-bus">
                    <SelectInput
                      id="vm-bus"
                      value={draft.bus}
                      mono
                      onChange={(v) => set("bus", v as DiskBus)}
                    >
                      <option value="virtio-blk">virtio-blk</option>
                      <option value="virtio-scsi">virtio-scsi</option>
                      <option value="sata">sata</option>
                    </SelectInput>
                  </Field>
                </div>
                <label className="flex items-center gap-[10px] cursor-pointer select-none">
                  <Switch on={draft.discard} onChange={(v) => set("discard", v)} />
                  <span className="text-[13px] text-[var(--qz-fg-2)]">
                    Pass discard through to the pool
                  </span>
                </label>
              </>
            )}
          </>
        )}

        {step === "network" && (
          <>
            {bridges === null ? (
              <p className="text-[13px] text-[var(--qz-fg-4)] m-0">Reading the node's bridges…</p>
            ) : bridges.length === 0 ? (
              <div className="callout callout-warn">
                <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
                <div className="text-[13px] text-[var(--qz-fg-2)]">
                  This node has no bridges, so the machine will be created without an adapter.
                  Create one under Networking → Interfaces first if it needs the network.
                </div>
              </div>
            ) : (
              <>
                <Field label="Bridge" htmlFor="vm-bridge" error={errors.bridge}>
                  <SelectInput
                    id="vm-bridge"
                    value={draft.bridge}
                    mono
                    invalid={!!errors.bridge}
                    onChange={(v) => set("bridge", v)}
                  >
                    {bridges.map((bridge) => (
                      <option key={bridge} value={bridge}>
                        {bridge}
                      </option>
                    ))}
                  </SelectInput>
                </Field>
                <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
                  <Field label="Model" htmlFor="vm-nic-model">
                    <SelectInput
                      id="vm-nic-model"
                      value={draft.nicModel}
                      mono
                      onChange={(v) => set("nicModel", v as NicModel)}
                    >
                      <option value="virtio">virtio</option>
                      <option value="e1000e">e1000e</option>
                      <option value="rtl8139">rtl8139</option>
                    </SelectInput>
                  </Field>
                  <Field label="VLAN tag" htmlFor="vm-vlan" error={errors.vlan_tag}>
                    <TextInput
                      id="vm-vlan"
                      value={draft.vlanTag}
                      mono
                      inputMode="numeric"
                      invalid={!!errors.vlan_tag}
                      placeholder="none"
                      onChange={(v) => set("vlanTag", v)}
                    />
                  </Field>
                </div>
              </>
            )}
          </>
        )}

        {step === "review" && (
          <>
            <dl className="qz-facts">
              <dt>Name</dt>
              <dd className="qz-mono">{draft.name}</dd>
              <dt>Processors</dt>
              <dd>{draft.vcpus}</dd>
              <dt>Memory</dt>
              <dd>{draft.memoryMib} MiB</dd>
              <dt>Firmware</dt>
              <dd>{draft.firmware === "uefi" ? "UEFI" : "Legacy BIOS"}</dd>
              <dt>Disk</dt>
              <dd className="qz-mono">
                {draft.pool ? `${draft.pool} · ${draft.sizeGib} GiB · ${draft.bus}` : "none"}
              </dd>
              <dt>Network</dt>
              <dd className="qz-mono">
                {draft.bridge
                  ? `${draft.bridge} · ${draft.nicModel}${draft.vlanTag ? ` · VLAN ${draft.vlanTag}` : ""}`
                  : "none"}
              </dd>
            </dl>
            <label className="flex items-center gap-[10px] cursor-pointer select-none">
              <Switch on={draft.startNow} onChange={(v) => set("startNow", v)} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">Start it once it is defined</span>
            </label>
            {errors.form && <ErrorText msg={errors.form} />}
          </>
        )}

        <div className="flex gap-2 justify-end mt-1">
          <Button kind="ghost" onClick={onClose} disabled={saving}>
            Cancel
          </Button>
          {index > 0 && (
            <Button kind="secondary" onClick={back} disabled={saving}>
              Back
            </Button>
          )}
          <Button kind="primary" type="submit" disabled={saving}>
            {step === "review" ? (saving ? "Creating…" : "Create") : "Next"}
          </Button>
        </div>
      </form>
    </ModalShell>
  );
}

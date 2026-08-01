"use client";

import { useState } from "react";
import { GripVertical } from "lucide-react";
import { Panel } from "@/components/vm/VmBits";
import { Button } from "@/components/ui/Button";
import { Switch } from "@/components/ui/Switch";
import { ErrorText, Field, TextInput } from "@/components/ui/formkit";
import { updateVm, validationErrorsOf, type BootDevice, type VmView } from "@/lib/vmClient";

const BOOT_LABEL: Record<BootDevice, string> = {
  disk: "Hard Disk",
  cdrom: "CD/DVD Drive",
  network: "Network (PXE)",
};

/// One row of the boot list: a device class the machine has, in the order the
/// firmware will try it, or switched out of the running entirely.
interface BootEntry {
  device: BootDevice;
  enabled: boolean;
}

/// The saved order first — those boot, in that order — then every class the
/// machine has devices for but does not boot, switched off at the bottom.
const bootEntriesOf = (vm: VmView): BootEntry[] => {
  const present: BootDevice[] = [];
  if (vm.disks.length > 0) present.push("disk");
  if (vm.cdroms.length > 0) present.push("cdrom");
  if (vm.nics.length > 0) present.push("network");
  const entries: BootEntry[] = vm.boot_order.map((device) => ({ device, enabled: true }));
  for (const device of present) {
    if (!entries.some((entry) => entry.device === device)) {
      entries.push({ device, enabled: false });
    }
  }
  return entries;
};

/// The settings that are about the machine rather than about its hardware:
/// what it is called, what it is for, and whether the node starts it.
export function VmOptions({
  vm,
  busy,
  onChanged,
}: {
  vm: VmView;
  busy: boolean;
  onChanged: (message: string) => Promise<void> | void;
}) {
  const [name, setName] = useState(vm.name);
  const [description, setDescription] = useState(vm.description ?? "");
  const [tags, setTags] = useState(vm.tags.join(", "));
  const [startOnBoot, setStartOnBoot] = useState(vm.start_on_boot);
  const [guestAgent, setGuestAgent] = useState(vm.guest_agent);
  const [ha, setHa] = useState(vm.ha);
  // Here rather than on the hardware page, the way Proxmox files it: boot
  // order is about the machine, not about any one device — each hardware
  // dialog edits exactly the thing its row names.
  const [bootEntries, setBootEntries] = useState<BootEntry[]>(() => bootEntriesOf(vm));
  /// The index being dragged, while a drag is in flight.
  const [dragging, setDragging] = useState<number | null>(null);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  const bootOrder = bootEntries
    .filter((entry) => entry.enabled)
    .map((entry) => entry.device);

  const dirty =
    name !== vm.name ||
    description !== (vm.description ?? "") ||
    tags !== vm.tags.join(", ") ||
    startOnBoot !== vm.start_on_boot ||
    guestAgent !== vm.guest_agent ||
    ha !== vm.ha ||
    bootOrder.join(",") !== vm.boot_order.join(",");

  const submit = async () => {
    setErrors({});
    setSaving(true);
    try {
      const response = await updateVm(vm.vmid, {
        name: name.trim(),
        description: description.trim(),
        tags: tags
          .split(",")
          .map((tag) => tag.trim())
          .filter(Boolean),
        start_on_boot: startOnBoot,
        guest_agent: guestAgent,
        ha,
        boot_order: bootOrder,
      });
      const pending = response.pending_reboot;
      await onChanged(
        pending.length > 0 ? `Saved. ${pending.join("; ")}` : `${response.vm.name} saved.`,
      );
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

  const reset = () => {
    setName(vm.name);
    setDescription(vm.description ?? "");
    setTags(vm.tags.join(", "));
    setStartOnBoot(vm.start_on_boot);
    setGuestAgent(vm.guest_agent);
    setHa(vm.ha);
    setBootEntries(bootEntriesOf(vm));
    setErrors({});
  };

  return (
    <form
      className="flex flex-col gap-4"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
    >
      <Panel title="Options">
        <div className="flex flex-col gap-4">
          <Field
            label="Name"
            htmlFor="opt-name"
            required
            error={errors.name}
            hint={
              vm.state === "running"
                ? "A machine can only be renamed while it is stopped."
                : undefined
            }
          >
            <TextInput
              id="opt-name"
              value={name}
              mono
              readOnly={vm.state === "running"}
              invalid={!!errors.name}
              onChange={setName}
            />
          </Field>

          <Field label="Description" htmlFor="opt-description">
            <TextInput
              id="opt-description"
              value={description}
              placeholder="What this machine is for"
              onChange={setDescription}
            />
          </Field>

          <Field
            label="Tags"
            htmlFor="opt-tags"
            hint="Comma separated. Lower-cased and sorted when saved."
            error={errors.tags}
          >
            <TextInput
              id="opt-tags"
              value={tags}
              mono
              invalid={!!errors.tags}
              placeholder="production, web"
              onChange={setTags}
            />
          </Field>

          <Field
            label="Boot Order"
            hint="Drag to reorder; the firmware tries each in turn and falls through anything
                  it cannot boot — so a blank disk ahead of the installer still boots the
                  installer, and boots itself once the install is done."
            error={errors.boot_order}
          >
            <ul className="m-0 p-0 flex flex-col gap-1" style={{ listStyle: "none" }}>
              {bootEntries.map((entry, index) => (
                <li
                  key={entry.device}
                  draggable
                  onDragStart={() => setDragging(index)}
                  onDragEnd={() => setDragging(null)}
                  onDragOver={(event) => {
                    // Reorder live as the row is carried over its neighbours,
                    // so the list previews exactly what dropping gives.
                    event.preventDefault();
                    if (dragging === null || dragging === index) return;
                    setBootEntries((entries) => {
                      const next = [...entries];
                      const [moved] = next.splice(dragging, 1);
                      next.splice(index, 0, moved);
                      return next;
                    });
                    setDragging(index);
                  }}
                  className="surface flex items-center gap-2 px-3 py-2 select-none"
                  style={{
                    cursor: "grab",
                    opacity: dragging === index ? 0.5 : entry.enabled ? 1 : 0.6,
                  }}
                >
                  <GripVertical size={14} className="flex-shrink-0 text-[var(--qz-fg-4)]" />
                  <span className="w-5 text-center text-[12px] qz-mono text-[var(--qz-fg-4)]">
                    {entry.enabled
                      ? bootEntries.filter((e) => e.enabled).indexOf(entry) + 1
                      : "—"}
                  </span>
                  <span className="text-[13px] text-[var(--qz-fg-1)] flex-1">
                    {BOOT_LABEL[entry.device]}
                  </span>
                  <Switch
                    on={entry.enabled}
                    onChange={(on) =>
                      setBootEntries((entries) =>
                        entries.map((e) =>
                          e.device === entry.device ? { ...e, enabled: on } : e,
                        ),
                      )
                    }
                  />
                </li>
              ))}
            </ul>
          </Field>

          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <Switch on={startOnBoot} onChange={setStartOnBoot} />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              Start this machine when the node boots
            </span>
          </label>

          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <Switch on={guestAgent} onChange={setGuestAgent} />
            <span className="text-[13px] text-[var(--qz-fg-2)]">Guest agent channel</span>
          </label>

          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <Switch on={ha} onChange={setHa} />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              High availability — restart on a surviving member after this node is confirmed
              lost. Needs every disk replicated.
            </span>
          </label>

          {errors.management && <ErrorText msg={errors.management} />}
          {errors.form && <ErrorText msg={errors.form} />}

          <div className="flex gap-2 justify-end">
            <Button kind="ghost" onClick={reset} disabled={saving || !dirty}>
              Revert
            </Button>
            <Button kind="primary" type="submit" disabled={busy || saving || !dirty}>
              {saving ? "Saving…" : "Save"}
            </Button>
          </div>
        </div>
      </Panel>
    </form>
  );
}

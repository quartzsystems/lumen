"use client";

import { useState } from "react";
import { Panel } from "@/components/vm/VmBits";
import { Button } from "@/components/ui/Button";
import { Switch } from "@/components/ui/Switch";
import { ErrorText, Field, TextInput } from "@/components/ui/formkit";
import { updateVm, validationErrorsOf, type BootDevice, type VmView } from "@/lib/vmClient";

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
  // Here rather than on the hardware page, the way Proxmox files it: boot
  // order is about the machine, not about any one device — each hardware
  // dialog edits exactly the thing its row names.
  const [bootOrder, setBootOrder] = useState(vm.boot_order.join(", "));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  const dirty =
    name !== vm.name ||
    description !== (vm.description ?? "") ||
    tags !== vm.tags.join(", ") ||
    startOnBoot !== vm.start_on_boot ||
    guestAgent !== vm.guest_agent ||
    bootOrder !== vm.boot_order.join(", ");

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
        boot_order: bootOrder
          .split(",")
          .map((entry) => entry.trim())
          .filter(Boolean) as BootDevice[],
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
    setBootOrder(vm.boot_order.join(", "));
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
            label="Boot order"
            htmlFor="opt-boot"
            hint="Comma separated: cdrom, disk, network. A device the machine does not have is ignored."
            error={errors.boot_order}
          >
            <TextInput
              id="opt-boot"
              value={bootOrder}
              mono
              invalid={!!errors.boot_order}
              placeholder="disk, network"
              onChange={setBootOrder}
            />
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

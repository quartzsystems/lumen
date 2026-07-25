"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Upload, X } from "lucide-react";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { Switch } from "@/components/ui/Switch";
import { Tabs, type TabItem } from "@/components/ui/Tabs";
import { ErrorText, Field, SelectInput, TextInput } from "@/components/ui/formkit";
import { Button } from "@/components/ui/Button";
import { fetchInterfaces } from "@/lib/networkClient";
import {
  createIsoStore,
  fetchIsos,
  fetchPools,
  uploadIso,
  type IsoStoreView,
  type IsoView,
  type PoolView,
} from "@/lib/storageClient";
import {
  createVm,
  fetchCpuModels,
  fetchNextVmid,
  fetchOsCatalog,
  formatBytes,
  validationErrorsOf,
  type BootDevice,
  type CdromCreate,
  type CpuModel,
  type CpuModels,
  type DiskBus,
  type Firmware,
  type NicModel,
  type OsCatalog,
  type OsVariant,
  type VmView,
} from "@/lib/vmClient";

type Tab = "general" | "os" | "system" | "disks" | "cpu" | "memory" | "network" | "confirm";

/// The tabs, in the order Proxmox established — which is the order the
/// decisions actually depend on each other in, and the order an operator
/// coming from Proxmox already knows.
const TABS: { id: Tab; label: string }[] = [
  { id: "general", label: "General" },
  { id: "os", label: "OS" },
  { id: "system", label: "System" },
  { id: "disks", label: "Disks" },
  { id: "cpu", label: "CPU" },
  { id: "memory", label: "Memory" },
  { id: "network", label: "Network" },
  { id: "confirm", label: "Confirm" },
];

/// Which tab a rejected field belongs to, so a validation error from the
/// backend lands on the tab that can fix it rather than wherever the operator
/// happened to be.
const TAB_OF_FIELD: Record<string, Tab> = {
  name: "general",
  vmid: "general",
  description: "general",
  os_id: "os",
  cdroms: "os",
  firmware: "system",
  machine: "system",
  pool: "disks",
  size_gib: "disks",
  vcpus: "cpu",
  topology: "cpu",
  cpu_model: "cpu",
  memory_mib: "memory",
  bridge: "network",
  vlan_tag: "network",
};

/// Every field the dialog collects. Numbers stay strings so a half-typed one
/// is a normal input state rather than NaN — the rule `LinkDialog` follows.
interface Draft {
  name: string;
  vmid: string;
  description: string;
  startOnBoot: boolean;
  osId: string;
  osFamily: string;
  mediaStorage: string;
  mediaImage: string;
  useMedia: boolean;
  virtioStorage: string;
  virtioImage: string;
  addVirtio: boolean;
  firmware: Firmware;
  machine: string;
  guestAgent: boolean;
  pool: string;
  sizeGib: string;
  bus: DiskBus;
  discard: boolean;
  sockets: string;
  cores: string;
  cpuType: string;
  memoryMib: string;
  bridge: string;
  nicModel: NicModel;
  vlanTag: string;
  startNow: boolean;
}

/// The two host-derived processor modes, which are not named models and so are
/// not in the hypervisor's model list.
const HOST_MODEL = "host_model";
const HOST_PASSTHROUGH = "host_passthrough";

const emptyDraft = (): Draft => ({
  name: "",
  vmid: "",
  description: "",
  startOnBoot: false,
  osId: "",
  osFamily: "",
  mediaStorage: "",
  mediaImage: "",
  useMedia: true,
  virtioStorage: "",
  virtioImage: "",
  addVirtio: false,
  firmware: "uefi",
  machine: "q35",
  guestAgent: true,
  pool: "",
  sizeGib: "32",
  bus: "virtio-blk",
  discard: true,
  sockets: "1",
  cores: "2",
  cpuType: HOST_MODEL,
  memoryMib: "4096",
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

/// A file matching `virtio-win*.iso`, which is what the driver disc is always
/// called wherever it came from. Used to pre-select it rather than to restrict
/// the choice — an operator who keeps it under another name picks it by hand.
const looksLikeVirtio = (name: string): boolean => /^virtio-win.*\.iso$/i.test(name);

/// Create a machine.
///
/// Tabs rather than a wizard, and every one of them reachable at any time: the
/// eight groups are decisions an operator makes in roughly this order, but
/// "roughly" is the point — coming back to change the memory after picking a
/// disk should not mean walking through four screens. A tab with something
/// wrong on it is marked; nothing is gated behind anything else.
export function CreateVmDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (vm: VmView) => void;
}) {
  const [tab, setTab] = useState<Tab>("general");
  const [draft, setDraft] = useState<Draft>(emptyDraft);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [saving, setSaving] = useState(false);

  const [pools, setPools] = useState<PoolView[] | null>(null);
  const [bridges, setBridges] = useState<string[] | null>(null);
  const [isos, setIsos] = useState<IsoView[] | null>(null);
  const [isoStores, setIsoStores] = useState<IsoStoreView[]>([]);
  const [catalog, setCatalog] = useState<OsCatalog | null>(null);
  const [cpus, setCpus] = useState<CpuModels | null>(null);

  const set = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((d) => ({ ...d, [key]: value }));

  // Everything the pickers offer is what the node actually has. A machine
  // pointed at a pool, a bridge, or an image that is not there defines cleanly
  // and then cannot start.
  const loadIsos = useCallback(async () => {
    try {
      const response = await fetchIsos();
      setIsos(response.images);
      setIsoStores(response.stores);
      setDraft((d) => ({
        ...d,
        mediaStorage: d.mediaStorage || (response.stores[0]?.storage ?? ""),
        virtioStorage: d.virtioStorage || (response.stores[0]?.storage ?? ""),
        // Pre-select the driver disc if one is already on the node, so the
        // Windows path is one tick rather than a hunt.
        virtioImage:
          d.virtioImage || (response.images.find((i) => looksLikeVirtio(i.name))?.name ?? ""),
      }));
    } catch {
      setIsos([]);
    }
  }, []);

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
      try {
        // Advisory, not a reservation — see the endpoint. Shown so the
        // identifier is visible before anything is created, and editable
        // because an operator may want a specific one.
        const { vmid } = await fetchNextVmid();
        setDraft((d) => ({ ...d, vmid: d.vmid || String(vmid) }));
      } catch {
        /* the backend allocates one anyway */
      }
      try {
        const found = await fetchOsCatalog();
        setCatalog(found);
        setDraft((d) => ({ ...d, osFamily: d.osFamily || (found.families[0]?.id ?? "") }));
      } catch {
        setCatalog({ families: [], source: "", reason: "The guest list could not be read." });
      }
      try {
        setCpus(await fetchCpuModels());
      } catch {
        setCpus({ host_passthrough: true, models: [], reason: "unreadable" });
      }
      await loadIsos();
    })();
  }, [loadIsos]);

  const selectedPool = useMemo(
    () => pools?.find((pool) => pool.name === draft.pool) ?? null,
    [pools, draft.pool],
  );

  const family = useMemo(
    () => catalog?.families.find((f) => f.id === draft.osFamily) ?? null,
    [catalog, draft.osFamily],
  );

  const selectedOs: OsVariant | null = useMemo(
    () => family?.variants.find((v) => v.id === draft.osId) ?? null,
    [family, draft.osId],
  );

  // The one thing the guest choice changes: a Windows installer has no virtio
  // driver, so it needs the driver disc in a second drive.
  const wantsVirtio = selectedOs?.needs_virtio_drivers ?? false;
  useEffect(() => {
    setDraft((d) => (d.addVirtio === wantsVirtio ? d : { ...d, addVirtio: wantsVirtio }));
  }, [wantsVirtio]);

  const imagesIn = useCallback(
    (storage: string) => (isos ?? []).filter((image) => image.storage === storage),
    [isos],
  );

  const totalCores = (numberOf(draft.sockets) ?? 0) * (numberOf(draft.cores) ?? 0);

  // --- validation ------------------------------------------------------------
  // Every tab is checked every time, so the marks on the tab bar are always
  // current and nothing is discovered only at the end.
  const validate = useCallback((d: Draft): Record<string, string> => {
    const found: Record<string, string> = {};
    if (!/^[A-Za-z0-9._-]{1,63}$/.test(d.name) || d.name.startsWith("-")) {
      found.name = "Use up to 63 letters, digits, dots, dashes, or underscores.";
    }
    if (d.vmid.trim() !== "") {
      const vmid = numberOf(d.vmid);
      if (vmid === undefined || vmid < 100 || vmid > 999_999_999) {
        found.vmid = "Use an identifier of 100 or more.";
      }
    }
    if (d.useMedia && (d.mediaStorage === "") !== (d.mediaImage === "")) {
      found.cdroms = "Choose an image, or turn the drive off.";
    }
    if (d.addVirtio && (d.virtioStorage === "") !== (d.virtioImage === "")) {
      found.virtio = "Choose the driver image, or turn the drive off.";
    }
    const sockets = numberOf(d.sockets);
    const cores = numberOf(d.cores);
    if (sockets === undefined || sockets < 1) found.sockets = "At least one socket.";
    if (cores === undefined || cores < 1) found.cores = "At least one core.";
    const memory = numberOf(d.memoryMib);
    if (memory === undefined || memory < 128) found.memory_mib = "Use at least 128 MiB.";
    if (d.vlanTag.trim() !== "") {
      const tag = numberOf(d.vlanTag);
      if (tag === undefined || tag < 1 || tag > 4094) found.vlan_tag = "Use a tag from 1 to 4094.";
    }
    return found;
  }, []);

  // Disk validation needs the pool's free space, so it is separate from the
  // pure-draft rules above.
  const diskError = useMemo(() => {
    if (!draft.pool) return undefined;
    const size = numberOf(draft.sizeGib);
    if (size === undefined || size < 1) return "Use a size of at least 1 GiB.";
    if (selectedPool && size * 1024 ** 3 > selectedPool.free) {
      return `"${selectedPool.name}" has ${formatBytes(selectedPool.free)} free.`;
    }
    return undefined;
  }, [draft.pool, draft.sizeGib, selectedPool]);

  const live = useMemo(() => {
    const found = validate(draft);
    if (diskError) found.size_gib = diskError;
    return found;
  }, [draft, validate, diskError]);

  /// Which tabs have something wrong on them right now. Only fields the
  /// operator has actually touched count as wrong on the tab bar — an empty
  /// form should not open covered in red marks.
  const [touched, setTouched] = useState(false);
  const tabItems: TabItem[] = useMemo(() => {
    const shown = touched ? live : {};
    const marked = new Set(
      Object.keys(shown)
        .map((field) => TAB_OF_FIELD[field] ?? (field === "virtio" ? "os" : undefined))
        .filter(Boolean) as Tab[],
    );
    // `sockets` and `cores` are CPU-tab fields the map does not name.
    if (shown.sockets || shown.cores) marked.add("cpu");
    return TABS.map((t) => ({ value: t.id, label: t.label, invalid: marked.has(t.id) }));
  }, [live, touched]);

  const submit = async () => {
    setTouched(true);
    const found = validate(draft);
    if (diskError) found.size_gib = diskError;
    if (Object.keys(found).length > 0) {
      setErrors(found);
      const first = Object.keys(found)[0];
      setTab(TAB_OF_FIELD[first] ?? "general");
      return;
    }
    setErrors({});
    setSaving(true);
    try {
      const cdroms: CdromCreate[] = [];
      if (draft.useMedia && draft.mediaStorage && draft.mediaImage) {
        cdroms.push({ storage: draft.mediaStorage, image: draft.mediaImage });
      }
      if (draft.addVirtio && draft.virtioStorage && draft.virtioImage) {
        cdroms.push({ storage: draft.virtioStorage, image: draft.virtioImage });
      }
      // Media first when there is any: a machine created to install an
      // operating system should boot the installer, not the empty disk.
      const bootOrder: BootDevice[] =
        cdroms.length > 0 ? ["cdrom", "disk", "network"] : ["disk", "network"];

      const cpuModel: CpuModel =
        draft.cpuType === HOST_MODEL
          ? "host_model"
          : draft.cpuType === HOST_PASSTHROUGH
            ? "host_passthrough"
            : { named: draft.cpuType };

      const sockets = numberOf(draft.sockets) ?? 1;
      const cores = numberOf(draft.cores) ?? 1;

      const vm = await createVm({
        name: draft.name.trim(),
        vmid: numberOf(draft.vmid),
        description: draft.description.trim() || undefined,
        os_id: draft.osId || undefined,
        vcpus: sockets * cores,
        topology: { sockets, cores, threads: 1 },
        memory_mib: numberOf(draft.memoryMib),
        cpu_model: cpuModel,
        machine: draft.machine || undefined,
        firmware: draft.firmware,
        boot_order: bootOrder,
        guest_agent: draft.guestAgent,
        start_on_boot: draft.startOnBoot,
        cdroms,
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
        const found: Record<string, string> = {};
        for (const item of detail) found[item.field ?? "form"] = item.message;
        setErrors(found);
        const first = detail.find((item) => item.field && TAB_OF_FIELD[item.field]);
        if (first?.field) setTab(TAB_OF_FIELD[first.field]);
      } else {
        setErrors({ form: err instanceof Error ? err.message : "Something went wrong." });
      }
    } finally {
      setSaving(false);
    }
  };

  const errorOf = (field: string) => errors[field] ?? (touched ? live[field] : undefined);
  const mark = <K extends keyof Draft>(key: K, value: Draft[K]) => {
    setTouched(true);
    set(key, value);
  };

  // Wide enough for all eight tabs on one row with room for the marks that
  // appear on them, and for the two-column tab bodies underneath.
  return (
    <ModalShell onClose={onClose} maxWidth={760}>
      <ModalHeader
        title="Create Virtual Machine"
        subtitle="The machine is defined on this node as soon as you finish."
        onClose={onClose}
      />

      <Tabs items={tabItems} value={tab} onChange={(v) => setTab(v as Tab)} className="mb-5" />

      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        {/* Fixed height so moving between tabs does not make the dialog jump. */}
        <div className="flex flex-col gap-4 min-h-[300px]">
          {tab === "general" && (
            <>
              <div className="grid gap-4" style={{ gridTemplateColumns: "2fr 1fr" }}>
                <Field label="Name" htmlFor="vm-name" required error={errorOf("name")}>
                  <TextInput
                    id="vm-name"
                    value={draft.name}
                    mono
                    autoFocus
                    invalid={!!errorOf("name")}
                    placeholder="web01"
                    onChange={(v) => mark("name", v)}
                  />
                </Field>
                <Field
                  label="VM ID"
                  htmlFor="vm-id"
                  hint="The next free one."
                  error={errorOf("vmid")}
                >
                  <TextInput
                    id="vm-id"
                    value={draft.vmid}
                    mono
                    inputMode="numeric"
                    invalid={!!errorOf("vmid")}
                    placeholder="100"
                    onChange={(v) => mark("vmid", v)}
                  />
                </Field>
              </div>
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
              <label className="flex items-center gap-[10px] cursor-pointer select-none">
                <Switch on={draft.startOnBoot} onChange={(v) => set("startOnBoot", v)} />
                <span className="text-[13px] text-[var(--qz-fg-2)]">Start on boot</span>
              </label>
            </>
          )}

          {tab === "os" && (
            <OsTab
              draft={draft}
              set={set}
              mark={mark}
              catalog={catalog}
              family={family}
              stores={isoStores}
              images={isos}
              imagesIn={imagesIn}
              errors={{ cdroms: errorOf("cdroms"), virtio: errorOf("virtio") }}
              onUploaded={loadIsos}
            />
          )}

          {tab === "system" && (
            <>
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
                  <option value="uefi">UEFI (OVMF)</option>
                  <option value="bios">Legacy BIOS (SeaBIOS)</option>
                </SelectInput>
              </Field>
              <Field
                label="Machine type"
                htmlFor="vm-machine"
                hint="q35 is the modern chipset. i440fx is for a guest too old to know it."
              >
                <SelectInput
                  id="vm-machine"
                  value={draft.machine}
                  mono
                  onChange={(v) => set("machine", v)}
                >
                  <option value="q35">q35</option>
                  <option value="pc">i440fx</option>
                </SelectInput>
              </Field>
              <label className="flex items-center gap-[10px] cursor-pointer select-none">
                <Switch on={draft.guestAgent} onChange={(v) => set("guestAgent", v)} />
                <span className="text-[13px] text-[var(--qz-fg-2)]">Guest agent channel</span>
              </label>
            </>
          )}

          {tab === "disks" && (
            <>
              {pools === null ? (
                <p className="text-[13px] text-[var(--qz-fg-4)] m-0">Reading the node&apos;s pools…</p>
              ) : pools.length === 0 ? (
                <Callout tone="warn">
                  This node has no storage pools, so the machine will be created without a disk. Add
                  one later from its Hardware page.
                </Callout>
              ) : (
                <>
                  <Field label="Storage" htmlFor="vm-pool" error={errorOf("pool")}>
                    <SelectInput
                      id="vm-pool"
                      value={draft.pool}
                      mono
                      invalid={!!errorOf("pool")}
                      onChange={(v) => mark("pool", v)}
                    >
                      {pools.map((pool) => (
                        <option key={pool.name} value={pool.name}>
                          {pool.name} — {formatBytes(pool.free)} free
                        </option>
                      ))}
                    </SelectInput>
                  </Field>
                  <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
                    <Field label="Disk size (GiB)" htmlFor="vm-size" required error={errorOf("size_gib")}>
                      <TextInput
                        id="vm-size"
                        value={draft.sizeGib}
                        mono
                        inputMode="numeric"
                        invalid={!!errorOf("size_gib")}
                        onChange={(v) => mark("sizeGib", v)}
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

          {tab === "cpu" && (
            <>
              <div className="grid gap-4 items-end" style={{ gridTemplateColumns: "1fr 1fr" }}>
                <Field label="Sockets" htmlFor="vm-sockets" required error={errorOf("sockets")}>
                  <TextInput
                    id="vm-sockets"
                    value={draft.sockets}
                    mono
                    inputMode="numeric"
                    invalid={!!errorOf("sockets")}
                    onChange={(v) => mark("sockets", v)}
                  />
                </Field>
                <Field label="Type" htmlFor="vm-cpu-type">
                  <SelectInput
                    id="vm-cpu-type"
                    value={draft.cpuType}
                    onChange={(v) => set("cpuType", v)}
                  >
                    <option value={HOST_MODEL}>
                      Default (host model
                      {cpus?.host_model ? ` — ${cpus.host_model}` : ""})
                    </option>
                    {cpus?.host_passthrough !== false && (
                      <option value={HOST_PASSTHROUGH}>Host passthrough</option>
                    )}
                    {(cpus?.models.length ?? 0) > 0 && (
                      <optgroup label="Runs on this node">
                        {cpus?.models
                          .filter((m) => m.usable)
                          .map((m) => (
                            <option key={m.name} value={m.name}>
                              {m.name}
                              {m.vendor ? ` (${m.vendor})` : ""}
                              {m.deprecated ? " — deprecated" : ""}
                            </option>
                          ))}
                      </optgroup>
                    )}
                    {(cpus?.models.some((m) => !m.usable) ?? false) && (
                      <optgroup label="Not runnable on this node">
                        {cpus?.models
                          .filter((m) => !m.usable)
                          .map((m) => (
                            <option key={m.name} value={m.name}>
                              {m.name}
                              {m.vendor ? ` (${m.vendor})` : ""}
                            </option>
                          ))}
                      </optgroup>
                    )}
                  </SelectInput>
                </Field>
                <Field label="Cores" htmlFor="vm-cores" required error={errorOf("cores")}>
                  <TextInput
                    id="vm-cores"
                    value={draft.cores}
                    mono
                    inputMode="numeric"
                    invalid={!!errorOf("cores")}
                    onChange={(v) => mark("cores", v)}
                  />
                </Field>
                <div className="text-[13px] text-[var(--qz-fg-3)] pb-[9px]">
                  Total cores{" "}
                  <span className="qz-mono font-semibold text-[var(--qz-accent)]">
                    {totalCores || "—"}
                  </span>
                </div>
              </div>
              {cpus === null && (
                <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
                  Reading what this node&apos;s processor can do…
                </p>
              )}
              {draft.cpuType !== HOST_MODEL &&
                draft.cpuType !== HOST_PASSTHROUGH &&
                cpus?.models.some((m) => m.name === draft.cpuType && !m.usable) && (
                  <Callout tone="warn">
                    This node cannot run <span className="qz-mono">{draft.cpuType}</span>. The
                    machine will be defined, but it will not start here.
                  </Callout>
                )}
            </>
          )}

          {tab === "memory" && (
            <Field
              label="Memory (MiB)"
              htmlFor="vm-memory"
              required
              error={errorOf("memory_mib")}
              hint="What the guest is given. The node keeps 2 GiB back for itself."
            >
              <TextInput
                id="vm-memory"
                value={draft.memoryMib}
                mono
                inputMode="numeric"
                invalid={!!errorOf("memory_mib")}
                onChange={(v) => mark("memoryMib", v)}
              />
            </Field>
          )}

          {tab === "network" && (
            <>
              {bridges === null ? (
                <p className="text-[13px] text-[var(--qz-fg-4)] m-0">
                  Reading the node&apos;s bridges…
                </p>
              ) : bridges.length === 0 ? (
                <Callout tone="warn">
                  This node has no bridges, so the machine will be created without an adapter.
                  Create one under Networking → Interfaces first if it needs the network.
                </Callout>
              ) : (
                <>
                  <Field label="Bridge" htmlFor="vm-bridge" error={errorOf("bridge")}>
                    <SelectInput
                      id="vm-bridge"
                      value={draft.bridge}
                      mono
                      invalid={!!errorOf("bridge")}
                      onChange={(v) => mark("bridge", v)}
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
                    <Field label="VLAN tag" htmlFor="vm-vlan" error={errorOf("vlan_tag")}>
                      <TextInput
                        id="vm-vlan"
                        value={draft.vlanTag}
                        mono
                        inputMode="numeric"
                        invalid={!!errorOf("vlan_tag")}
                        placeholder="none"
                        onChange={(v) => mark("vlanTag", v)}
                      />
                    </Field>
                  </div>
                </>
              )}
            </>
          )}

          {tab === "confirm" && (
            <>
              <dl className="qz-facts">
                <dt>Name</dt>
                <dd className="qz-mono">{draft.name || "—"}</dd>
                <dt>VM ID</dt>
                <dd className="qz-mono">{draft.vmid || "next free"}</dd>
                <dt>Guest</dt>
                <dd>{selectedOs?.name ?? "not recorded"}</dd>
                <dt>Media</dt>
                <dd className="qz-mono">
                  {draft.useMedia && draft.mediaImage
                    ? `${draft.mediaStorage}: ${draft.mediaImage}`
                    : "none"}
                  {draft.addVirtio && draft.virtioImage ? ` + ${draft.virtioImage}` : ""}
                </dd>
                <dt>Firmware</dt>
                <dd>{draft.firmware === "uefi" ? "UEFI" : "Legacy BIOS"}</dd>
                <dt>Processors</dt>
                <dd className="qz-mono">
                  {draft.sockets} × {draft.cores} = {totalCores} · {draft.cpuType}
                </dd>
                <dt>Memory</dt>
                <dd className="qz-mono">{draft.memoryMib} MiB</dd>
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
                <span className="text-[13px] text-[var(--qz-fg-2)]">
                  Start it once it is defined
                </span>
              </label>
            </>
          )}
        </div>

        {errors.form && <ErrorText msg={errors.form} />}

        {/* Create is available from every tab: there is nothing left to
            discover on the ones not visited, and their defaults are shown. */}
        <div className="flex gap-2 justify-end mt-1 pt-4 border-t border-[var(--qz-border)]">
          <Button kind="ghost" onClick={onClose} disabled={saving}>
            Cancel
          </Button>
          <Button kind="primary" type="submit" disabled={saving}>
            {saving ? "Creating…" : "Create"}
          </Button>
        </div>
      </form>
    </ModalShell>
  );
}

/// The guest and its installation media.
function OsTab({
  draft,
  set,
  mark,
  catalog,
  family,
  stores,
  images,
  imagesIn,
  errors,
  onUploaded,
}: {
  draft: Draft;
  set: <K extends keyof Draft>(key: K, value: Draft[K]) => void;
  mark: <K extends keyof Draft>(key: K, value: Draft[K]) => void;
  catalog: OsCatalog | null;
  family: { id: string; label: string; variants: OsVariant[] } | null;
  stores: IsoStoreView[];
  images: IsoView[] | null;
  imagesIn: (storage: string) => IsoView[];
  errors: { cdroms?: string; virtio?: string };
  onUploaded: () => Promise<void>;
}) {
  const store = stores.find((s) => s.storage === draft.mediaStorage) ?? null;

  return (
    <>
      {/* One grid rather than two independent columns, so the rows line up:
          the media heading sits level with the guest heading, and Storage with
          Type. Stacking each side separately left the right-hand fields riding
          higher than the left by exactly the height of the radios. */}
      <div className="grid gap-4 items-start" style={{ gridTemplateColumns: "1fr 1fr" }}>
        {/* The two answers to "what does this machine boot from" sit together;
            the storage and image that only matter to the first of them follow
            underneath. */}
        <div className="flex flex-col gap-[10px]">
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <input
              type="radio"
              checked={draft.useMedia}
              onChange={() => set("useMedia", true)}
              style={{ accentColor: "var(--qz-accent)" }}
            />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              Use CD/DVD disc image file (iso)
            </span>
          </label>
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <input
              type="radio"
              checked={!draft.useMedia}
              onChange={() => set("useMedia", false)}
              style={{ accentColor: "var(--qz-accent)" }}
            />
            <span className="text-[13px] text-[var(--qz-fg-2)]">Do not use any media</span>
          </label>
        </div>
        <div className="text-[13px] text-[var(--qz-fg-2)] font-semibold">Guest OS:</div>

        <div className={draft.useMedia ? "" : "opacity-40 pointer-events-none"}>
          <Field label="Storage" htmlFor="vm-media-storage">
            <SelectInput
              id="vm-media-storage"
              value={draft.mediaStorage}
              mono
              onChange={(v) => mark("mediaStorage", v)}
            >
              {stores.length === 0 && <option value="">no media library</option>}
              {stores.map((s) => (
                <option key={s.storage} value={s.storage}>
                  {s.storage}
                </option>
              ))}
            </SelectInput>
          </Field>
        </div>
        <Field label="Type" htmlFor="vm-os-family">
          <SelectInput
            id="vm-os-family"
            value={draft.osFamily}
            onChange={(v) => {
              set("osFamily", v);
              // The version list is per family, so the old choice is not in
              // it any more.
              set("osId", "");
            }}
          >
            {(catalog?.families.length ?? 0) === 0 && <option value="">unavailable</option>}
            {catalog?.families.map((f) => (
              <option key={f.id} value={f.id}>
                {f.label}
              </option>
            ))}
          </SelectInput>
        </Field>

        <div className={draft.useMedia ? "" : "opacity-40 pointer-events-none"}>
          <Field label="ISO image" htmlFor="vm-media-image" error={errors.cdroms}>
            <SelectInput
              id="vm-media-image"
              value={draft.mediaImage}
              mono
              invalid={!!errors.cdroms}
              onChange={(v) => mark("mediaImage", v)}
            >
              <option value="">Choose an image…</option>
              {imagesIn(draft.mediaStorage).map((image) => (
                <option key={image.name} value={image.name}>
                  {image.name} — {formatBytes(image.size)}
                </option>
              ))}
            </SelectInput>
          </Field>
        </div>
        <div className="flex flex-col gap-4">
          <Field label="Version" htmlFor="vm-os-version">
            <SelectInput id="vm-os-version" value={draft.osId} onChange={(v) => set("osId", v)}>
              <option value="">Not specified</option>
              {groupByVendor(family?.variants ?? []).map(([vendor, variants]) => (
                <optgroup key={vendor} label={vendor}>
                  {variants.map((variant) => (
                    <option key={variant.id} value={variant.id}>
                      {variant.name}
                      {variant.end_of_life ? " (end of life)" : ""}
                    </option>
                  ))}
                </optgroup>
              ))}
            </SelectInput>
          </Field>
          {catalog?.reason && (
            <p className="text-[11px] text-[var(--qz-fg-4)] m-0">{catalog.reason}</p>
          )}
        </div>
      </div>

      {/* Only for a guest whose installer has no virtio driver — which is
          Windows, and is the one thing the guest choice actually changes. */}
      {draft.addVirtio && (
        <div className="flex flex-col gap-4 pt-4 border-t border-[var(--qz-border)]">
          <label className="flex items-center gap-[10px] cursor-pointer select-none">
            <input
              type="checkbox"
              checked={draft.addVirtio}
              onChange={(e) => set("addVirtio", e.target.checked)}
              style={{ accentColor: "var(--qz-accent)" }}
            />
            <span className="text-[13px] text-[var(--qz-fg-2)]">
              Add additional drive for VirtIO drivers
            </span>
          </label>
          <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
            <Field label="Storage" htmlFor="vm-virtio-storage">
              <SelectInput
                id="vm-virtio-storage"
                value={draft.virtioStorage}
                mono
                onChange={(v) => mark("virtioStorage", v)}
              >
                {stores.length === 0 && <option value="">no media library</option>}
                {stores.map((s) => (
                  <option key={s.storage} value={s.storage}>
                    {s.storage}
                  </option>
                ))}
              </SelectInput>
            </Field>
            <Field label="ISO image" htmlFor="vm-virtio-image" error={errors.virtio}>
              <SelectInput
                id="vm-virtio-image"
                value={draft.virtioImage}
                mono
                invalid={!!errors.virtio}
                onChange={(v) => mark("virtioImage", v)}
              >
                <option value="">Choose the driver image…</option>
                {imagesIn(draft.virtioStorage).map((image) => (
                  <option key={image.name} value={image.name}>
                    {image.name} — {formatBytes(image.size)}
                  </option>
                ))}
              </SelectInput>
            </Field>
          </div>
          <p className="text-[11px] text-[var(--qz-fg-4)] m-0">
            A Windows installer cannot see a virtio disk or adapter without these drivers. Load them
            from this drive when the installer asks where to install.
          </p>
        </div>
      )}

      {images !== null && images.length === 0 && (
        <Callout tone={store?.ready === false ? "warn" : "info"}>
          {store?.ready === false
            ? store.reason
            : "There are no installation images on this node yet. Upload one below, or copy files into the media library on the node."}
        </Callout>
      )}

      <IsoUploader
        storage={draft.mediaStorage}
        stores={stores}
        onUploaded={onUploaded}
        onStoreCreated={onUploaded}
      />
    </>
  );
}

/// Put an installation image on the node from here.
function IsoUploader({
  storage,
  stores,
  onUploaded,
  onStoreCreated,
}: {
  storage: string;
  stores: IsoStoreView[];
  onUploaded: () => Promise<void>;
  onStoreCreated: () => Promise<void>;
}) {
  const input = useRef<HTMLInputElement>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const abort = useRef<(() => void) | null>(null);
  const store = stores.find((s) => s.storage === storage) ?? null;

  const start = async (file: File) => {
    setError(null);
    setProgress(0);
    const upload = uploadIso(storage, file, setProgress);
    abort.current = upload.abort;
    try {
      await upload.promise;
      await onUploaded();
    } catch (err) {
      setError(err instanceof Error ? err.message : "The upload failed.");
    } finally {
      setProgress(null);
      abort.current = null;
      if (input.current) input.current.value = "";
    }
  };

  if (store && !store.ready) {
    return (
      <div className="flex items-center gap-3 pt-4 border-t border-[var(--qz-border)]">
        <span className="text-[12px] text-[var(--qz-fg-4)] flex-1">
          No media library on <span className="qz-mono">{storage}</span> yet.
        </span>
        <Button
          kind="secondary"
          size="sm"
          onClick={() => {
            void (async () => {
              setError(null);
              try {
                await createIsoStore(storage);
                await onStoreCreated();
              } catch (err) {
                setError(err instanceof Error ? err.message : "Could not create the library.");
              }
            })();
          }}
        >
          Create it
        </Button>
        {error && <ErrorText msg={error} />}
      </div>
    );
  }

  return (
    <div className="flex items-center gap-3 pt-4 border-t border-[var(--qz-border)]">
      <input
        ref={input}
        type="file"
        accept=".iso,application/x-iso9660-image,application/octet-stream"
        className="hidden"
        onChange={(e) => {
          const file = e.target.files?.[0];
          if (file) void start(file);
        }}
      />
      {progress === null ? (
        <>
          <Button
            kind="secondary"
            size="sm"
            icon={Upload}
            onClick={() => input.current?.click()}
            disabled={!storage}
          >
            Upload image
          </Button>
          <span className="text-[12px] text-[var(--qz-fg-4)] flex-1">
            {storage ? (
              <>
                Goes to <span className="qz-mono">{storage}</span>. Large files take a while — the
                dialog stays open.
              </>
            ) : (
              "No media library to upload to."
            )}
          </span>
          {error && <ErrorText msg={error} />}
        </>
      ) : (
        <>
          <div className="flex-1 h-[6px] rounded-full overflow-hidden" style={{ background: "var(--qz-input-bg)" }}>
            <div
              className="h-full transition-[width] duration-150"
              style={{ width: `${Math.round(progress * 100)}%`, background: "var(--qz-accent)" }}
            />
          </div>
          <span className="text-[12px] text-[var(--qz-fg-3)] qz-mono">
            {Math.round(progress * 100)}%
          </span>
          <button
            type="button"
            onClick={() => abort.current?.()}
            className="text-[var(--qz-fg-4)] hover:text-[var(--qz-fg-1)] transition-colors cursor-pointer bg-transparent border-0 p-0"
            title="Cancel the upload"
          >
            <X size={15} />
          </button>
        </>
      )}
    </div>
  );
}

/// Long families are unusable as one flat list, so the version drop-down is
/// grouped the way the database already groups them.
function groupByVendor(variants: OsVariant[]): [string, OsVariant[]][] {
  const groups = new Map<string, OsVariant[]>();
  for (const variant of variants) {
    const key = variant.vendor ?? "Other";
    const found = groups.get(key);
    if (found) found.push(variant);
    else groups.set(key, [variant]);
  }
  return [...groups.entries()];
}

function Callout({ tone, children }: { tone: "warn" | "info"; children: React.ReactNode }) {
  return (
    <div className={tone === "warn" ? "callout callout-warn" : "callout"}>
      <AlertTriangle
        size={17}
        className="flex-shrink-0 mt-[1px]"
        style={{ color: tone === "warn" ? "var(--qz-warn)" : "var(--qz-fg-4)" }}
      />
      <div className="text-[13px] text-[var(--qz-fg-2)]">{children}</div>
    </div>
  );
}

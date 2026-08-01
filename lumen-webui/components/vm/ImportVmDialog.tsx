"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, Trash2, Upload, X } from "lucide-react";

import { ProgressRow } from "@/components/cluster/CreateClusterDialog";
import { Button } from "@/components/ui/Button";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { Switch } from "@/components/ui/Switch";
import { Tabs, type TabItem } from "@/components/ui/Tabs";
import { ErrorText, Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  commitImport,
  deleteImport,
  fetchImportPending,
  fetchImports,
  uploadOva,
  type ImportProgress,
  type ImportView,
  type OvaAppliance,
} from "@/lib/importClient";
import { fetchInterfaces } from "@/lib/networkClient";
import { fetchPooledStorage } from "@/lib/poolClient";
import { fetchPools } from "@/lib/storageClient";
import {
  fetchNextVmid,
  formatBytes,
  NIC_MODEL_LABEL,
  SCSI_CONTROLLER_LABEL,
  type DiskBus,
  type Firmware,
  type NicModel,
} from "@/lib/vmClient";

/// The disk-target choice that means "the cluster's replicated storage"
/// rather than a named local pool — the Create dialog's convention.
const REPLICATED_STORAGE = "__replicated__";

const POLL_MS = 1000;

/// Where one archive disk lands, as the form holds it.
interface DiskChoice {
  /// A pool name, or REPLICATED_STORAGE.
  target: string;
  bus: DiskBus;
}

/// What one source adapter becomes. An empty bridge leaves it behind.
interface NicChoice {
  bridge: string;
  model: NicModel;
  vlanTag: string;
}

/// The archive's own name for the machine, bent to Lumen's naming rule —
/// a proposal the operator edits, not a judgement.
const proposeName = (raw: string): string =>
  raw
    .replace(/[^A-Za-z0-9._-]+/g, "-")
    .replace(/^[-.]+/, "")
    .slice(0, 63);

/// Import a machine from a VMware archive.
///
/// Three stages under three tabs: the archive (upload one, or pick one
/// already in the spool), the machine (the archive's proposal, corrected —
/// which bridge each source network maps to, which pool each disk lands
/// in), and the import itself, watched step by step because filling disks
/// from an archive takes minutes and a spinner would say nothing.
export function ImportVmDialog({
  onClose,
  onImported,
}: {
  onClose: () => void;
  onImported: (vmid: number | null) => void;
}) {
  const [tab, setTab] = useState("archive");

  // The spool, and the upload into it.
  const [spooled, setSpooled] = useState<ImportView[] | null>(null);
  const [uploadPct, setUploadPct] = useState<number | null>(null);
  const [archiveError, setArchiveError] = useState<string | null>(null);
  const abort = useRef<(() => void) | null>(null);
  const input = useRef<HTMLInputElement>(null);

  // The chosen archive and the proposal built from it.
  const [archive, setArchive] = useState<{ name: string; appliance: OvaAppliance } | null>(null);
  const [name, setName] = useState("");
  const [vmid, setVmid] = useState("");
  const [vcpus, setVcpus] = useState("1");
  const [memoryMib, setMemoryMib] = useState("2048");
  const [firmware, setFirmware] = useState<Firmware>("bios");
  const [disks, setDisks] = useState<DiskChoice[]>([]);
  const [nics, setNics] = useState<NicChoice[]>([]);
  const [startNow, setStartNow] = useState(false);
  const [keepSource, setKeepSource] = useState(false);

  // What the node offers the pickers.
  const [pools, setPools] = useState<string[] | null>(null);
  const [hasPool, setHasPool] = useState(false);
  const [bridges, setBridges] = useState<string[] | null>(null);

  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [progress, setProgress] = useState<ImportProgress | null>(null);

  const loadSpool = useCallback(async () => {
    try {
      setSpooled((await fetchImports()).imports);
    } catch {
      setSpooled([]);
    }
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const response = await fetchPools();
        setPools(response.nodes.flatMap((node) => node.pools).map((pool) => pool.name));
      } catch {
        setPools([]);
      }
      try {
        const response = await fetchInterfaces();
        setBridges(
          response.nodes
            .flatMap((node) => node.interfaces)
            .filter((link) => link.kind === "bridge")
            .map((link) => link.name),
        );
      } catch {
        setBridges([]);
      }
      try {
        const pooled = await fetchPooledStorage();
        setHasPool(pooled.pool !== null);
      } catch {
        /* a standalone node simply has no replicated choice */
      }
      try {
        const { vmid } = await fetchNextVmid();
        setVmid((current) => current || String(vmid));
      } catch {
        /* the backend allocates one anyway */
      }
      await loadSpool();
    })();
  }, [loadSpool]);

  /// An archive is in hand — seed the whole form from what it says, with
  /// the node's first pool and bridge standing in until the operator says
  /// otherwise.
  const adopt = useCallback(
    (chosen: { name: string; appliance: OvaAppliance }) => {
      setArchive(chosen);
      const appliance = chosen.appliance;
      setName(proposeName(appliance.name));
      setVcpus(String(appliance.vcpus));
      setMemoryMib(String(appliance.memory_mib));
      setFirmware(appliance.firmware);
      const pool = pools?.[0] ?? "";
      setDisks(
        appliance.disks.map((disk) => ({
          target: pool || (hasPool ? REPLICATED_STORAGE : ""),
          bus: disk.bus,
        })),
      );
      const bridge = bridges?.[0] ?? "";
      setNics(
        appliance.nics.map((nic) => ({ bridge, model: nic.model, vlanTag: "" })),
      );
      setTab("machine");
    },
    [pools, bridges, hasPool],
  );

  const startUpload = async (file: File) => {
    setArchiveError(null);
    setUploadPct(0);
    const upload = uploadOva(file, setUploadPct);
    abort.current = upload.abort;
    try {
      const answer = await upload.promise;
      await loadSpool();
      adopt({ name: answer.name, appliance: answer.appliance });
    } catch (err) {
      setArchiveError(err instanceof Error ? err.message : "The upload failed.");
    } finally {
      setUploadPct(null);
      abort.current = null;
      if (input.current) input.current.value = "";
    }
  };

  // --- validation ------------------------------------------------------------

  const nameError = !/^[A-Za-z0-9._-]{1,63}$/.test(name) || name.startsWith("-")
    ? "Use up to 63 letters, digits, dots, dashes, or underscores."
    : undefined;
  const vmidError =
    vmid.trim() !== "" && (!/^\d+$/.test(vmid.trim()) || Number(vmid) < 100)
      ? "Use an identifier of 100 or more."
      : undefined;
  const vcpusError = !/^\d+$/.test(vcpus.trim()) || Number(vcpus) < 1
    ? "At least one processor."
    : undefined;
  const memoryError = !/^\d+$/.test(memoryMib.trim()) || Number(memoryMib) < 128
    ? "Use at least 128 MiB."
    : undefined;
  const disksError = disks.some((disk) => disk.target === "")
    ? "Choose where every disk lands."
    : undefined;
  const vlanError = nics.some((nic) => {
    if (nic.bridge === "" || nic.vlanTag.trim() === "") return false;
    const tag = Number(nic.vlanTag);
    return !Number.isInteger(tag) || tag < 1 || tag > 4094;
  })
    ? "VLAN tags run from 1 to 4094."
    : undefined;
  const machineReady =
    archive !== null &&
    !nameError &&
    !vmidError &&
    !vcpusError &&
    !memoryError &&
    !disksError &&
    !vlanError;

  // --- the commit and the watch ----------------------------------------------

  const submit = async () => {
    if (!archive || !machineReady) return;
    setSubmitting(true);
    setSubmitError(null);
    try {
      const started = await commitImport(archive.name, {
        name: name.trim(),
        vmid: vmid.trim() === "" ? undefined : Number(vmid),
        vcpus: Number(vcpus),
        memory_mib: Number(memoryMib),
        firmware,
        disks: disks.map((disk) =>
          disk.target === REPLICATED_STORAGE
            ? { replicated: true, bus: disk.bus }
            : { pool: disk.target, bus: disk.bus },
        ),
        nics: nics.map((nic) =>
          nic.bridge === ""
            ? {}
            : {
                bridge: nic.bridge,
                model: nic.model,
                vlan_tag: nic.vlanTag.trim() === "" ? undefined : Number(nic.vlanTag),
              },
        ),
        start: startNow,
        keep_source: keepSource,
      });
      setProgress(started);
      setTab("import");
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setSubmitError(err instanceof Error ? err.message : "The import was refused.");
    } finally {
      setSubmitting(false);
    }
  };

  const running = progress?.phase === "running";
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => {
      void (async () => {
        try {
          setProgress(await fetchImportPending());
        } catch {
          // A dropped poll is the next tick's problem.
        }
      })();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [running]);

  const busy = submitting || running || uploadPct !== null;
  const guard = busy ? () => {} : onClose;

  const tabs: TabItem[] = [
    { value: "archive", label: "Archive" },
    ...(archive ? [{ value: "machine", label: "Machine" }] : []),
    ...(progress ? [{ value: "import", label: "Import" }] : []),
  ];

  const appliance = archive?.appliance ?? null;

  return (
    <ModalShell onClose={guard} maxWidth={720}>
      <ModalHeader
        title="Import Virtual Machine"
        subtitle="A VMware appliance archive (.ova) becomes a machine on this node, disks and all."
        onClose={guard}
      />
      <Tabs items={tabs} value={tab} onChange={setTab} className="mb-4" />

      {tab === "archive" && (
        <div className="flex flex-col gap-4">
          <input
            ref={input}
            type="file"
            accept=".ova,application/octet-stream"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) void startUpload(file);
            }}
          />
          {uploadPct === null ? (
            <div className="flex items-center gap-3">
              <Button kind="secondary" size="sm" icon={Upload} onClick={() => input.current?.click()}>
                Upload archive
              </Button>
              <span className="text-[12px] text-[var(--qz-fg-4)] flex-1">
                An export takes minutes to upload — the dialog stays open, and the archive waits on
                the node if you come back later.
              </span>
            </div>
          ) : (
            <div className="flex items-center gap-3">
              <div
                className="flex-1 h-[6px] rounded-full overflow-hidden"
                style={{ background: "var(--qz-input-bg)" }}
              >
                <div
                  className="h-full transition-[width] duration-150"
                  style={{ width: `${Math.round(uploadPct * 100)}%`, background: "var(--qz-accent)" }}
                />
              </div>
              <span className="text-[12px] text-[var(--qz-fg-3)] qz-mono">
                {Math.round(uploadPct * 100)}%
              </span>
              <button
                type="button"
                onClick={() => abort.current?.()}
                className="text-[var(--qz-fg-4)] hover:text-[var(--qz-fg-1)] transition-colors cursor-pointer bg-transparent border-0 p-0"
                title="Cancel the upload"
              >
                <X size={15} />
              </button>
            </div>
          )}
          {archiveError && <ErrorText msg={archiveError} />}

          {spooled !== null && spooled.length > 0 && (
            <section className="flex flex-col gap-1">
              <h3 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
                Already on the node
              </h3>
              <div
                className="rounded-md overflow-hidden"
                style={{ border: "1px solid var(--qz-border)" }}
              >
                {spooled.map((view) => (
                  <div
                    key={view.name}
                    className="flex items-center gap-3 px-3 py-2"
                    style={{ borderTop: "1px solid var(--qz-border)" }}
                  >
                    <span className="qz-mono text-[12px] text-[var(--qz-fg-2)]">{view.name}</span>
                    <span className="text-[12px] text-[var(--qz-fg-4)] flex-1">
                      {view.appliance
                        ? `${view.appliance.name} — ${view.appliance.vcpus} vCPU, ${formatBytes(
                            view.appliance.memory_mib * 1024 * 1024,
                          )}, ${view.appliance.disks.length} disk(s)`
                        : (view.error ?? "unreadable")}
                    </span>
                    {view.appliance && (
                      <Button
                        kind="secondary"
                        size="sm"
                        onClick={() => adopt({ name: view.name, appliance: view.appliance! })}
                      >
                        Import this
                      </Button>
                    )}
                    <button
                      type="button"
                      onClick={() => {
                        void (async () => {
                          try {
                            await deleteImport(view.name);
                            await loadSpool();
                          } catch (err) {
                            setArchiveError(
                              err instanceof Error ? err.message : "Could not remove it.",
                            );
                          }
                        })();
                      }}
                      className="text-[var(--qz-fg-4)] hover:text-[var(--qz-danger)] transition-colors cursor-pointer bg-transparent border-0 p-0"
                      title="Remove the archive from the node"
                    >
                      <Trash2 size={14} />
                    </button>
                  </div>
                ))}
              </div>
            </section>
          )}
        </div>
      )}

      {tab === "machine" && appliance && (
        <form
          className="flex flex-col gap-4"
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          {appliance.warnings.length > 0 && (
            <div className="callout callout-warn">
              <AlertTriangle
                size={17}
                className="flex-shrink-0 mt-[1px]"
                style={{ color: "var(--qz-warn)" }}
              />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {appliance.warnings.map((warning) => (
                  <div key={warning}>{warning}</div>
                ))}
              </div>
            </div>
          )}

          <div className="grid gap-4" style={{ gridTemplateColumns: "2fr 1fr" }}>
            <Field label="Name" htmlFor="import-name" required error={nameError}>
              <TextInput id="import-name" value={name} onChange={setName} invalid={!!nameError} />
            </Field>
            <Field label="VM ID" htmlFor="import-vmid" error={vmidError}>
              <TextInput
                id="import-vmid"
                value={vmid}
                onChange={setVmid}
                mono
                invalid={!!vmidError}
              />
            </Field>
          </div>

          <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr 1fr" }}>
            <Field label="Processors" htmlFor="import-vcpus" error={vcpusError}>
              <TextInput id="import-vcpus" value={vcpus} onChange={setVcpus} invalid={!!vcpusError} />
            </Field>
            <Field label="Memory (MiB)" htmlFor="import-memory" error={memoryError}>
              <TextInput
                id="import-memory"
                value={memoryMib}
                onChange={setMemoryMib}
                invalid={!!memoryError}
              />
            </Field>
            <Field
              label="Firmware"
              htmlFor="import-firmware"
              hint="What the source booted with. Changing it usually means the machine will not boot."
            >
              <SelectInput
                id="import-firmware"
                value={firmware}
                onChange={(value) => setFirmware(value as Firmware)}
              >
                <option value="uefi">UEFI</option>
                <option value="bios">BIOS (SeaBIOS)</option>
              </SelectInput>
            </Field>
          </div>

          <section className="flex flex-col gap-1">
            <h3 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">Disks</h3>
            <div className="rounded-md overflow-hidden" style={{ border: "1px solid var(--qz-border)" }}>
              {appliance.disks.map((disk, index) => (
                <div
                  key={disk.file}
                  className="flex items-center gap-3 px-3 py-2"
                  style={{ borderTop: "1px solid var(--qz-border)" }}
                >
                  <span className="qz-mono text-[12px] text-[var(--qz-fg-2)]">{disk.file}</span>
                  <span className="text-[12px] text-[var(--qz-fg-4)] flex-1">
                    {formatBytes(disk.capacity)}
                  </span>
                  <span style={{ width: 160 }}>
                    <SelectInput
                      value={disks[index]?.target ?? ""}
                      onChange={(value) =>
                        setDisks((current) =>
                          current.map((d, i) => (i === index ? { ...d, target: value } : d)),
                        )
                      }
                      invalid={disks[index]?.target === ""}
                    >
                      {disks[index]?.target === "" && <option value="">Choose storage…</option>}
                      {pools?.map((pool) => (
                        <option key={pool} value={pool}>
                          {pool}
                        </option>
                      ))}
                      {hasPool && <option value={REPLICATED_STORAGE}>Replicated storage</option>}
                    </SelectInput>
                  </span>
                  <span style={{ width: 130 }}>
                    <SelectInput
                      value={disks[index]?.bus ?? disk.bus}
                      onChange={(value) =>
                        setDisks((current) =>
                          current.map((d, i) =>
                            i === index ? { ...d, bus: value as DiskBus } : d,
                          ),
                        )
                      }
                    >
                      <option value="virtio-scsi">SCSI</option>
                      <option value="sata">SATA</option>
                      <option value="virtio-blk">VirtIO Block</option>
                    </SelectInput>
                  </span>
                </div>
              ))}
            </div>
            <span className="text-[12px] text-[var(--qz-fg-4)]">
              {disksError ??
                (appliance.scsi_controller
                  ? `SCSI disks keep the source's ${SCSI_CONTROLLER_LABEL[appliance.scsi_controller]} controller, so the guest's driver still finds them. Switch to VirtIO once its drivers are installed.`
                  : "The buses proposed are the ones the source machine used.")}
            </span>
          </section>

          {appliance.nics.length > 0 && (
            <section className="flex flex-col gap-1">
              <h3 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
                Network adapters
              </h3>
              <div
                className="rounded-md overflow-hidden"
                style={{ border: "1px solid var(--qz-border)" }}
              >
                {appliance.nics.map((nic, index) => (
                  <div
                    key={`${nic.network}-${index}`}
                    className="flex items-center gap-3 px-3 py-2"
                    style={{ borderTop: "1px solid var(--qz-border)" }}
                  >
                    <span className="text-[12px] text-[var(--qz-fg-2)] flex-1" title="The source's network">
                      {nic.network || "(unnamed network)"}
                      {nic.mac && (
                        <span className="qz-mono text-[11px] text-[var(--qz-fg-4)] block">
                          {nic.mac} — kept
                        </span>
                      )}
                    </span>
                    <span style={{ width: 150 }}>
                      <SelectInput
                        value={nics[index]?.bridge ?? ""}
                        onChange={(value) =>
                          setNics((current) =>
                            current.map((n, i) => (i === index ? { ...n, bridge: value } : n)),
                          )
                        }
                      >
                        <option value="">Leave behind</option>
                        {bridges?.map((bridge) => (
                          <option key={bridge} value={bridge}>
                            {bridge}
                          </option>
                        ))}
                      </SelectInput>
                    </span>
                    <span style={{ width: 150 }}>
                      <SelectInput
                        value={nics[index]?.model ?? nic.model}
                        onChange={(value) =>
                          setNics((current) =>
                            current.map((n, i) =>
                              i === index ? { ...n, model: value as NicModel } : n,
                            ),
                          )
                        }
                      >
                        {Object.entries(NIC_MODEL_LABEL).map(([value, label]) => (
                          <option key={value} value={value}>
                            {label}
                          </option>
                        ))}
                      </SelectInput>
                    </span>
                    <span style={{ width: 80 }}>
                      <TextInput
                        value={nics[index]?.vlanTag ?? ""}
                        onChange={(value) =>
                          setNics((current) =>
                            current.map((n, i) => (i === index ? { ...n, vlanTag: value } : n)),
                          )
                        }
                        placeholder="VLAN"
                        mono
                      />
                    </span>
                  </div>
                ))}
              </div>
              {vlanError && <ErrorText msg={vlanError} />}
            </section>
          )}

          <div className="flex items-center gap-6">
            <label className="flex items-center gap-2 text-[13px] text-[var(--qz-fg-2)]">
              <Switch on={startNow} onChange={setStartNow} />
              Start after import
            </label>
            <label className="flex items-center gap-2 text-[13px] text-[var(--qz-fg-2)]">
              <Switch on={keepSource} onChange={setKeepSource} />
              Keep the archive for another import
            </label>
          </div>

          {submitError && <div className="callout callout-crit">{submitError}</div>}
          <ModalFooter
            onCancel={onClose}
            saving={submitting}
            disabled={!machineReady}
            submitLabel="Import"
            savingLabel="Starting…"
          />
        </form>
      )}

      {tab === "import" && progress && (
        <div className="flex flex-col gap-4">
          {progress.phase === "failed" && (
            <div className="callout callout-crit">{progress.error ?? "The import failed."}</div>
          )}
          <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
            {progress.steps.map((step, index) => (
              <ProgressRow key={`${step.step}-${index}`} step={step} />
            ))}
          </ul>
          {progress.phase === "complete" && (
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              The machine is imported{startNow ? " and starting" : ""}.
            </div>
          )}
          {progress.phase !== "running" && (
            <div className="flex justify-end gap-2">
              <Button kind="secondary" onClick={onClose}>
                Close
              </Button>
              {progress.phase === "complete" && (
                <Button kind="primary" onClick={() => onImported(progress.vmid ?? null)}>
                  Open the machine
                </Button>
              )}
            </div>
          )}
        </div>
      )}
    </ModalShell>
  );
}

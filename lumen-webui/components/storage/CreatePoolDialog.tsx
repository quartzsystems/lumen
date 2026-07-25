"use client";

import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, HardDrive } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  CheckList,
  CheckRow,
  ErrorText,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
} from "@/components/ui/formkit";
import { Switch } from "@/components/ui/Switch";
import { formatBytes, validationErrorsOf } from "@/lib/vmClient";
import {
  createPool,
  fetchDevices,
  usableBytes,
  VDEV_INFO,
  type BlockDevice,
  type Compression,
  type PoolView,
  type VdevKind,
} from "@/lib/storageClient";

/// Build a pool.
///
/// The most destructive thing this console can be asked to do: every disk
/// chosen is reformatted and there is no undo. So the picker reports what is
/// already on each disk, a disk that has something on it cannot be chosen
/// without the acknowledgement, and the estimate underneath says what the
/// arrangement actually costs before anything is run.
export function CreatePoolDialog({
  existingNames,
  onClose,
  onCreated,
}: {
  existingNames: string[];
  onClose: () => void;
  onCreated: (pool: PoolView) => void;
}) {
  const [devices, setDevices] = useState<BlockDevice[] | null>(null);
  const [loadError, setLoadError] = useState("");

  const [name, setName] = useState("");
  const [vdev, setVdev] = useState<VdevKind>("mirror");
  const [chosen, setChosen] = useState<string[]>([]);
  const [compression, setCompression] = useState<Compression>("lz4");
  const [autotrim, setAutotrim] = useState(true);
  const [acked, setAcked] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});

  // Read once when the dialog opens. The node's disks do not change while
  // somebody is describing a pool, and re-reading `/sys/block` under a cursor
  // would move the rows.
  useEffect(() => {
    let cancelled = false;
    fetchDevices()
      .then((response) => !cancelled && setDevices(response.devices))
      .catch((err) => {
        if (cancelled) return;
        setLoadError(err instanceof Error ? err.message : "Could not read this node's disks.");
        setDevices([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const all = devices ?? [];
  const picked = useMemo(
    () => all.filter((device) => chosen.includes(device.path)),
    [all, chosen],
  );
  // Any chosen disk that already holds something. This is what turns the
  // acknowledgement on, and the reason it names the disk.
  const occupied = picked.filter((device) => device.in_use);
  const smallest = picked.length > 0 ? Math.min(...picked.map((device) => device.size)) : 0;
  const spec = VDEV_INFO[vdev];
  const enough = picked.length >= spec.minDisks;

  const toggle = (path: string) =>
    setChosen((current) =>
      current.includes(path) ? current.filter((p) => p !== path) : [...current, path],
    );

  const submit = async () => {
    setSaving(true);
    setError("");
    setFieldErrors({});
    try {
      onCreated(
        await createPool({
          name: name.trim(),
          vdev,
          disks: chosen,
          compression,
          autotrim,
          i_understand_this_may_lose_data: acked,
        }),
      );
    } catch (err) {
      const problems: Record<string, string> = {};
      for (const problem of validationErrorsOf(err)) {
        if (problem.field && !problems[problem.field]) problems[problem.field] = problem.message;
      }
      setFieldErrors(problems);
      setError(
        Object.keys(problems).length > 0
          ? ""
          : err instanceof Error
            ? err.message
            : "Could not create the pool.",
      );
      setSaving(false);
    }
  };

  const duplicate = existingNames.some((existing) => existing === name.trim());

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title="Create a storage pool"
        subtitle="Every disk chosen is reformatted. There is no undo."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <Field
          label="Name"
          htmlFor="pool-name"
          required
          error={fieldErrors.name ?? (duplicate ? "This node already has a pool with that name." : undefined)}
          hint="Letters, digits, and _ - . : — starting with a letter."
        >
          <TextInput
            id="pool-name"
            value={name}
            onChange={setName}
            mono
            autoFocus
            invalid={!!fieldErrors.name || duplicate}
            placeholder="tank"
          />
        </Field>

        <Field label="Arrangement" htmlFor="pool-vdev" hint={spec.note}>
          <SelectInput id="pool-vdev" value={vdev} onChange={(value) => setVdev(value as VdevKind)}>
            {(Object.keys(VDEV_INFO) as VdevKind[]).map((kind) => (
              <option key={kind} value={kind}>
                {VDEV_INFO[kind].label} — {VDEV_INFO[kind].minDisks}+ disks
              </option>
            ))}
          </SelectInput>
        </Field>

        <Field
          label="Disks"
          error={fieldErrors.disks}
          hint={
            enough
              ? undefined
              : `${spec.label} needs at least ${spec.minDisks} disk${spec.minDisks === 1 ? "" : "s"}.`
          }
        >
          {devices === null ? (
            <p className="text-[13px] text-[var(--qz-fg-4)] m-0">Reading this node&rsquo;s disks…</p>
          ) : all.length === 0 ? (
            <p className="text-[13px] text-[var(--qz-fg-4)] m-0">
              {loadError || "This node reports no disks a pool could be built on."}
            </p>
          ) : (
            <CheckList>
              {all.map((device) => (
                <CheckRow
                  key={device.path}
                  checked={chosen.includes(device.path)}
                  onChange={() => toggle(device.path)}
                >
                  <span className="flex items-center gap-2 min-w-0 flex-1">
                    <HardDrive size={14} className="flex-shrink-0 text-[var(--qz-fg-4)]" />
                    <span className="qz-mono text-[12.5px] text-[var(--qz-fg-1)]">
                      {device.name}
                    </span>
                    <span className="text-[12px] text-[var(--qz-fg-3)]">
                      {formatBytes(device.size)}
                    </span>
                    {device.model && (
                      <span className="text-[11px] text-[var(--qz-fg-4)] truncate">
                        {device.model}
                      </span>
                    )}
                    {!device.rotational && <span className="badge badge-muted">SSD</span>}
                    {device.in_use && (
                      <span className="badge badge-warn" title={device.used_by}>
                        {device.used_by ?? "in use"}
                      </span>
                    )}
                  </span>
                </CheckRow>
              ))}
            </CheckList>
          )}
        </Field>

        {picked.length > 0 && (
          <div className="qz-facts">
            <dt>Usable</dt>
            <dd>
              {formatBytes(usableBytes(vdev, picked.length, smallest))}
              <span className="qz-dim">
                {" "}
                of {formatBytes(smallest * picked.length)} — before ZFS&rsquo;s own overhead
              </span>
            </dd>
            <dt>Survives</dt>
            <dd>
              {spec.parity === 0 ? (
                <span style={{ color: "var(--qz-warn)" }}>no disk failing</span>
              ) : (
                `${spec.parity} disk${spec.parity === 1 ? "" : "s"} failing`
              )}
            </dd>
            {picked.some((device) => device.size !== smallest) && (
              <>
                <dt>Note</dt>
                <dd className="qz-dim">
                  The disks are not the same size, so every one is used as if it were the smallest.
                </dd>
              </>
            )}
          </div>
        )}

        <details>
          <summary className="text-[12px] text-[var(--qz-fg-3)] cursor-pointer select-none">
            Advanced
          </summary>
          <div className="flex flex-col gap-4 mt-3">
            <Field
              label="Compression"
              htmlFor="pool-compression"
              hint="lz4 is fast enough to be free on any processor this appliance runs on."
            >
              <SelectInput
                id="pool-compression"
                value={compression}
                onChange={(value) => setCompression(value as Compression)}
              >
                <option value="lz4">lz4</option>
                <option value="zstd">zstd — better ratios, more processor</option>
                <option value="off">off</option>
              </SelectInput>
            </Field>
            <label className="flex items-center gap-[10px] cursor-pointer select-none">
              <Switch on={autotrim} onChange={setAutotrim} />
              <span className="text-[13px] text-[var(--qz-fg-2)]">
                Automatic TRIM <span className="qz-dim">— tells solid-state disks what is free</span>
              </span>
            </label>
            <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
              Sector size is fixed at 4 KiB (<span className="qz-mono">ashift=12</span>), which every
              disk made this decade actually has. It cannot be changed after the pool exists, and a
              pool built at 512 bytes on a 4 KiB disk is slow forever.
            </p>
          </div>
        </details>

        {occupied.length > 0 && (
          <>
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {occupied.length === 1 ? "This disk has" : "These disks have"} something on{" "}
                {occupied.length === 1 ? "it" : "them"} already, and building a pool destroys it:
                <ul className="m-0 mt-1 pl-4">
                  {occupied.map((device) => (
                    <li key={device.path}>
                      <span className="qz-mono">{device.name}</span> — {device.used_by}
                    </li>
                  ))}
                </ul>
              </div>
            </div>
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
          </>
        )}

        <ErrorText msg={error} />
        <ModalFooter
          onCancel={onClose}
          saving={saving}
          disabled={
            !name.trim() || duplicate || !enough || (occupied.length > 0 && !acked)
          }
          savingLabel="Creating…"
          submitLabel="Create pool"
          onSubmit={submit}
        />
      </div>
    </ModalShell>
  );
}

/// Destroy one, and everything on it.
export function DestroyPoolDialog({
  pool,
  onClose,
  onConfirm,
  busy,
}: {
  pool: PoolView;
  busy: boolean;
  onClose: () => void;
  onConfirm: (acknowledge: boolean) => void;
}) {
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  // Typing the pool's name is friction that fits the action: a checkbox is
  // ticked without reading, and this cannot be done to the wrong pool by
  // accident — which is the mistake that actually happens.
  const confirmed = typed.trim() === pool.name && acked;

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Destroy ${pool.name}?`}
        subtitle="Every dataset, volume, and snapshot on it goes with it."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {formatBytes(pool.allocated)} is allocated on this pool. Destroying it removes every
            virtual machine disk on it and everything else it holds. There is no undo, and no
            snapshot to go back to — the snapshots are on the pool.
          </div>
        </div>
        <Field label={`Type ${pool.name} to confirm`} htmlFor="destroy-pool-name">
          <TextInput
            id="destroy-pool-name"
            value={typed}
            onChange={setTyped}
            mono
            autoFocus
            placeholder={pool.name}
          />
        </Field>
        <label className="flex items-center gap-[10px] cursor-pointer select-none">
          <input
            type="checkbox"
            checked={acked}
            onChange={(e) => setAcked(e.target.checked)}
            style={{ accentColor: "var(--qz-accent)" }}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">I understand this may lose data.</span>
        </label>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          savingLabel="Destroying…"
          submitLabel="Destroy pool"
          onSubmit={() => onConfirm(acked)}
        />
      </div>
    </ModalShell>
  );
}

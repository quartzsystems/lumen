"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { AlertTriangle, HardDrive, Info } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  createClusterPool,
  devicesByMember,
  type InventoryResponse,
  type PoolSeat,
} from "@/lib/inventoryClient";
import { shortNodeName } from "@/lib/nodeNames";
import type { Compression, VdevKind } from "@/lib/storageClient";
import { formatBytes } from "@/lib/vmClient";

/// Choose disks across the members and build one pool on each of them.
///
/// The honest framing, said in the dialog and not only here: this does not
/// create one pool spanning the nodes. Each member gets its own pool of the
/// same name, on its own disks. That is what replicated volumes need — a
/// volume placed on two members wants a pool of that name on both — and
/// pretending otherwise would set an expectation the storage engine cannot
/// meet. docs/storage-scaleout.md is where a genuinely pooled address space
/// is costed out.
export function PoolAcrossNodesDialog({
  inventory,
  onClose,
  onCreated,
}: {
  inventory: InventoryResponse | null;
  onClose: () => void;
  onCreated: (message: string) => void;
}) {
  const [name, setName] = useState("tank");
  const [vdev, setVdev] = useState<VdevKind>("mirror");
  const [compression, setCompression] = useState<Compression>("lz4");
  /// Selected disks, keyed `node/path` — the pair, because two members may
  /// each have a disk at the same path and they are not the same disk.
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Only disks nothing has claimed. A disk that is in use is shown with what
  // holds it and cannot be picked: the one genuinely useful thing this dialog
  // can do is refuse to offer the disk the node boots from.
  const candidates = useMemo(() => devicesByMember(inventory), [inventory]);
  const nodes = useMemo(
    () => Array.from(new Set(candidates.map((row) => row.node))).sort(),
    [candidates],
  );

  /// Disks the picker has to refuse but an operator could reclaim: a
  /// partition table and nothing using it. Counted so the dialog can point at
  /// the page that clears them instead of just greying the row.
  ///
  /// `in_use` as well as `wipeable`, because clearing is now offered on disks
  /// that read as empty too — and those are already selectable here, so
  /// counting them would send an operator to another page for nothing.
  const reclaimable = candidates.filter(
    (row) => row.device.in_use && row.device.wipeable,
  ).length;

  const key = (node: string, path: string) => `${node}/${path}`;
  const toggle = (node: string, path: string) => {
    const next = new Set(picked);
    const id = key(node, path);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setPicked(next);
  };

  const seats: PoolSeat[] = nodes
    .map((node) => ({
      node,
      disks: candidates
        .filter((row) => row.node === node && picked.has(key(node, row.device.path)))
        .map((row) => row.device.path),
    }))
    .filter((seat) => seat.disks.length > 0);

  /// What each vdev layout needs, and what it survives. Checked per member,
  /// because a pool is built per member and a layout that works on one node's
  /// disk count and not another's is a half-finished cluster.
  const minimumFor = (kind: VdevKind): number =>
    kind === "mirror" ? 2 : kind === "raidz1" ? 3 : kind === "raidz2" ? 4 : kind === "raidz3" ? 5 : 1;

  const short = seats.filter((seat) => seat.disks.length < minimumFor(vdev));
  const ready =
    name.trim() !== "" && seats.length > 0 && short.length === 0 && acked && !busy;

  // Raw, and labelled raw wherever it is shown: a replicated volume costs its
  // full size on every member holding a replica.
  const rawTotal = seats.reduce((sum, seat) => {
    const perDisk = candidates
      .filter((row) => row.node === seat.node && seat.disks.includes(row.device.path))
      .map((row) => row.device.size);
    if (perDisk.length === 0) return sum;
    // Usable-per-member under the chosen layout, near enough to be useful:
    // a mirror is one disk's worth, raidzN loses N to parity, a stripe is all
    // of it. ZFS's real answer differs by a few percent and is not knowable
    // until the pool exists.
    const smallest = Math.min(...perDisk);
    const usable =
      vdev === "mirror"
        ? smallest
        : vdev === "stripe"
          ? perDisk.reduce((a, b) => a + b, 0)
          : smallest * (perDisk.length - minimumFor(vdev) + 1);
    return sum + Math.max(0, usable);
  }, 0);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const outcome = await createClusterPool({
        name: name.trim(),
        vdev,
        compression,
        seats,
        i_understand_this_erases_the_disks: true,
      });
      onCreated(`Pool "${outcome.name}" built on ${outcome.built.join(", ")}.`);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The pool could not be built.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title="Pool drives across nodes"
        subtitle="One pool name, built on each member from the disks you choose there."
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        {/* Said plainly and up front, because the word "pool" invites the
            other reading and an operator who expects one address space across
            the nodes will size their volumes wrong. */}
        <div className="callout">
          <HardDrive size={17} className="flex-shrink-0 text-[var(--qz-fg-4)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            Each member gets its own pool of this name, on its own disks — not one pool spanning
            the nodes. That is what a replicated volume needs: it names one pool and finds it on
            every member holding a replica. A volume can still be no larger than the member it
            sits on.
          </div>
        </div>

        <Field label="Pool name" htmlFor="cluster-pool-name" required>
          <TextInput id="cluster-pool-name" value={name} onChange={setName} mono autoFocus />
        </Field>

        <div className="grid gap-4" style={{ gridTemplateColumns: "1fr 1fr" }}>
          <Field
            label="Layout"
            htmlFor="cluster-pool-vdev"
            hint="Applied on every member, so each needs enough disks for it."
          >
            <SelectInput
              id="cluster-pool-vdev"
              value={vdev}
              onChange={(next) => setVdev(next as VdevKind)}
            >
              <option value="mirror">Mirror — survives losing a disk</option>
              <option value="raidz1">RAIDZ1 — one disk of parity</option>
              <option value="raidz2">RAIDZ2 — two disks of parity</option>
              <option value="raidz3">RAIDZ3 — three disks of parity</option>
              <option value="stripe">Stripe — no redundancy</option>
            </SelectInput>
          </Field>
          <Field label="Compression" htmlFor="cluster-pool-compression">
            <SelectInput
              id="cluster-pool-compression"
              value={compression}
              onChange={(next) => setCompression(next as Compression)}
            >
              <option value="lz4">lz4</option>
              <option value="zstd">zstd</option>
              <option value="off">off</option>
            </SelectInput>
          </Field>
        </div>

        {nodes.length === 0 ? (
          <div className="text-[13px] text-[var(--qz-fg-4)]">
            No member could be asked what disks it has.
          </div>
        ) : (
          nodes.map((node) => {
            const rows = candidates.filter((row) => row.node === node);
            const chosen = rows.filter((row) => picked.has(key(node, row.device.path))).length;
            return (
              <section key={node} className="flex flex-col gap-2">
                <div className="flex items-center gap-2">
                  <span
                    className="qz-mono text-[13px] font-semibold text-[var(--qz-fg-2)]"
                    title={node}
                  >
                    {shortNodeName(node)}
                  </span>
                  <span className="text-[12px] text-[var(--qz-fg-4)]">
                    {chosen} of {rows.length} chosen
                  </span>
                </div>
                <div className="surface flex flex-col">
                  {rows.map((row) => {
                    const id = key(node, row.device.path);
                    const disabled = row.device.in_use;
                    return (
                      <label
                        key={id}
                        className="qz-check-row px-3 py-2"
                        style={{
                          borderTop: "1px solid var(--qz-border)",
                          opacity: disabled ? 0.55 : 1,
                          cursor: disabled ? "not-allowed" : "pointer",
                        }}
                        title={row.device.used_by ?? undefined}
                      >
                        <input
                          type="checkbox"
                          className="qz-check"
                          checked={picked.has(id)}
                          disabled={disabled}
                          onChange={() => toggle(node, row.device.path)}
                        />
                        <span className="qz-mono text-[12px] text-[var(--qz-fg-2)]">
                          {row.device.name}
                        </span>
                        <span className="text-[12px] text-[var(--qz-fg-4)]">
                          {formatBytes(row.device.size)}
                          {row.device.model && ` · ${row.device.model}`}
                          {row.device.rotational ? " · spinning" : " · solid state"}
                        </span>
                        {/* A disk carrying nothing but an old partition table
                            is a decision away from usable, and saying only
                            "2 partitions" beside a greyed-out checkbox is
                            where an operator reusing hardware gets stuck.
                            The badge says which kind of unavailable it is. */}
                        {disabled && (
                          <span
                            className={`badge ml-auto ${row.device.wipeable ? "badge-warn" : "badge-muted"}`}
                          >
                            {row.device.used_by ?? "in use"}
                          </span>
                        )}
                      </label>
                    );
                  })}
                </div>
              </section>
            );
          })
        )}

        {/* The way out of the state this dialog can otherwise only refuse.
            A disk carrying an old partition table and nothing else can be
            cleared; one holding a pool or a mount cannot, and Disks is the
            page that draws that line. */}
        {reclaimable > 0 && (
          <div className="callout">
            <Info size={17} className="flex-shrink-0 text-[var(--qz-fg-4)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-3)]">
              {reclaimable} {reclaimable === 1 ? "disk carries" : "disks carry"} an old partition
              table and nothing using it. Clear{" "}
              {reclaimable === 1 ? "it" : "them"} on{" "}
              <Link href="/storage/disks" className="text-[var(--qz-accent)] no-underline">
                Storage → Disks
              </Link>{" "}
              and {reclaimable === 1 ? "it becomes" : "they become"} selectable here.
            </div>
          </div>
        )}

        {short.length > 0 && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              {short.map((seat) => seat.node).join(", ")} {short.length === 1 ? "has" : "have"} too
              few disks for {vdev} — it needs at least {minimumFor(vdev)} on every member it is
              built on.
            </div>
          </div>
        )}

        {seats.length > 0 && short.length === 0 && (
          <div className="text-[13px] text-[var(--qz-fg-3)]">
            About {formatBytes(rawTotal)} raw across {seats.length}{" "}
            {seats.length === 1 ? "member" : "members"}. Raw, not usable: a replicated volume
            costs its full size on every member holding a replica.
          </div>
        )}

        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as React.CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand every disk selected above is reformatted.
          </span>
        </label>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!ready}
          submitLabel="Build pools"
          savingLabel="Building…"
          onSubmit={() => void submit()}
        />
      </div>
    </ModalShell>
  );
}

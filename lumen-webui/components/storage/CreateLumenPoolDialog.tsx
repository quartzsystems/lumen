"use client";

import { useCallback, useEffect, useMemo, useState } from "react";

import { ProgressRow } from "@/components/cluster/CreateClusterDialog";
import { ModalShell, ModalHeader } from "@/components/ui/Modal";
import { Tabs, type TabItem } from "@/components/ui/Tabs";
import { ModalFooter, SelectInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { fetchEnvironment } from "@/lib/clusterClient";
import {
  devicesByMember,
  fetchInventory,
  type InventoryResponse,
} from "@/lib/inventoryClient";
import { shortNodeName } from "@/lib/nodeNames";
import {
  createLumenPool,
  fetchPoolPending,
  fetchPooledStorage,
  type LumenBrickSeat,
  type PoolProgress,
} from "@/lib/poolClient";
import { formatBytes } from "@/lib/vmClient";

/// The drive wizard: which disks become bricks, on which tier — the one
/// place pool creation happens, writing what phase 3 wrote by hand.
///
/// A tabbed dialog like every other creator in this console: every tab
/// stays reachable, an invalid tab wears a red dot, and the last tab is
/// the create itself — live per-member steps, because a wizard that
/// closes on submit turns a multi-minute workflow into a spinner. The
/// ending is unusual and deliberate: the coordinator adopts the pool by
/// restarting its own control plane, so when the feed dies mid-poll the
/// dialog switches to polling the observed pool itself and finishes on
/// the first answer that carries one — truth over feed.

const POLL_MS = 1000;

/// Tier choices the wizard offers. The engine accepts any u8; two classes
/// are what a two-member appliance meaningfully has.
const TIERS = [
  { value: 0, label: "Tier 0 — fast" },
  { value: 1, label: "Tier 1 — capacity" },
];

export function CreateLumenPoolDialog({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: () => void;
}) {
  const [tab, setTab] = useState("disks");
  const [inventory, setInventory] = useState<InventoryResponse | null>(null);
  const [members, setMembers] = useState<string[] | null>(null);
  const [clusterName, setClusterName] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  /// `${node}/${disk name}` → tier. A Map so a re-render never reorders
  /// what the operator is pointing at.
  const [picked, setPicked] = useState<Map<string, number>>(new Map());
  const [acked, setAcked] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [progress, setProgress] = useState<PoolProgress | null>(null);
  /// The coordinator is restarting itself to adopt the pool: the feed is
  /// gone, and the observed pool is what finishes the story.
  const [adopting, setAdopting] = useState(false);
  const [done, setDone] = useState(false);

  // Read once, on open — rows must not move under the cursor.
  useEffect(() => {
    void (async () => {
      try {
        const [fleet, environment] = await Promise.all([
          fetchInventory(),
          fetchEnvironment(),
        ]);
        setInventory(fleet);
        const home = environment.clusters.find((cluster) =>
          cluster.nodes.some((node) => node.local),
        );
        setMembers(home ? home.nodes.map((node) => node.node).sort() : []);
        setClusterName(home?.name ?? "");
      } catch (err) {
        if (err instanceof ApiError && err.status === 401) return;
        setLoadError(
          err instanceof Error ? err.message : "Could not read the environment.",
        );
      }
    })();
  }, []);

  const rows = useMemo(() => devicesByMember(inventory), [inventory]);
  const key = (node: string, disk: string) => `${node}/${disk}`;

  const toggle = (node: string, disk: string, rotational: boolean) => {
    setPicked((current) => {
      const next = new Map(current);
      const id = key(node, disk);
      if (next.has(id)) {
        next.delete(id);
      } else {
        // Seeded from what the disk is; the operator can overrule.
        next.set(id, rotational ? 1 : 0);
      }
      return next;
    });
  };

  const seats: LumenBrickSeat[] = useMemo(() => {
    if (!members) return [];
    return members.map((node) => ({
      node,
      bricks: rows
        .filter(
          (row) => row.node === node && picked.has(key(node, row.device.name)),
        )
        .map((row) => ({
          disk: row.device.name,
          tier: picked.get(key(node, row.device.name)) ?? 0,
        })),
    }));
  }, [members, rows, picked]);

  const seatValid = (seat: LumenBrickSeat) =>
    seat.bricks.length > 0 && seat.bricks.some((brick) => brick.tier === 0);
  const disksReady = seats.length > 0 && seats.every(seatValid);

  // The one figure, estimated from the chosen disks the same way the
  // server will state it: each member bounds the pool at its bytes times
  // the member count over two — every block lives on two of the members,
  // so at two this is the smaller member's truth and at three it exceeds
  // any one node. "About", because the format charges its own overheads
  // and the real seat split is off by a slice or two.
  const usableEstimate = useMemo(() => {
    if (!disksReady || !members) return null;
    const sizeOf = (node: string, disk: string) =>
      rows.find((row) => row.node === node && row.device.name === disk)?.device
        .size ?? 0;
    const tiers = new Set(seats.flatMap((s) => s.bricks.map((b) => b.tier)));
    let total = 0;
    for (const tier of tiers) {
      total += Math.min(
        ...seats.map(
          (seat) =>
            (seat.bricks
              .filter((brick) => brick.tier === tier)
              .reduce((sum, brick) => sum + sizeOf(seat.node, brick.disk), 0) *
              seats.length) /
            2,
        ),
      );
    }
    return total;
  }, [disksReady, members, seats, rows]);

  const submit = async () => {
    setSubmitting(true);
    setSubmitError(null);
    try {
      const started = await createLumenPool(seats);
      setProgress(started);
      setTab("create");
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setSubmitError(
        err instanceof Error ? err.message : "The create was refused.",
      );
    } finally {
      setSubmitting(false);
    }
  };

  // The feed, while it lives; the observed pool, once the coordinator's
  // own restart takes the feed with it.
  const running = progress?.phase === "running" && !adopting;
  const load = useCallback(async () => {
    try {
      const fresh = await fetchPoolPending();
      setProgress(fresh);
      if (fresh.phase === "complete") setAdopting(true);
    } catch {
      // A 404 or a dropped connection here is the restart, not a failure.
      setAdopting(true);
    }
  }, []);
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [running, load]);
  useEffect(() => {
    if (!adopting || done) return;
    const timer = setInterval(() => {
      void (async () => {
        try {
          const response = await fetchPooledStorage();
          if (response.pool) {
            setDone(true);
            onCreated();
          }
        } catch {
          // Still restarting; the next tick asks again.
        }
      })();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [adopting, done, onCreated]);

  const busy =
    submitting || (progress !== null && progress.phase !== "failed" && !done);
  const guard = busy ? () => {} : onClose;

  const tabs: TabItem[] = [
    {
      value: "disks",
      label: "Members & disks",
      count: picked.size,
      invalid: tab !== "disks" && !disksReady,
    },
    { value: "review", label: "Review", invalid: tab !== "review" && !acked },
    ...(progress ? [{ value: "create", label: "Create" }] : []),
  ];

  return (
    <ModalShell onClose={guard} maxWidth={720}>
      <ModalHeader
        title="Pooled storage"
        subtitle={
          clusterName
            ? `One deduplicated pool across ${clusterName}'s members — every disk chosen here becomes a brick.`
            : "One deduplicated pool across the cluster's members."
        }
        onClose={guard}
      />
      <Tabs items={tabs} value={tab} onChange={setTab} className="mb-4" />

      {tab === "disks" && (
        <div className="flex flex-col gap-4">
          {loadError && <div className="callout callout-crit">{loadError}</div>}
          {members !== null && members.length === 0 && (
            <div className="callout callout-warn text-[13px]">
              This node is not in a cluster, and a pool spans its cluster.
            </div>
          )}
          {members?.map((node) => {
            const own = rows.filter((row) => row.node === node);
            const chosen = own.filter((row) =>
              picked.has(key(node, row.device.name)),
            ).length;
            const seat = seats.find((s) => s.node === node);
            const tierless = seat && seat.bricks.length > 0 && !seatValid(seat);
            return (
              <section key={node} className="flex flex-col gap-1">
                <div className="flex items-baseline gap-2">
                  <h3 className="text-[13px] font-semibold text-[var(--qz-fg-2)] m-0">
                    {shortNodeName(node)}
                  </h3>
                  <span className="text-[12px] text-[var(--qz-fg-4)]">
                    {chosen} of {own.length} chosen
                  </span>
                  {tierless && (
                    <span className="text-[12px] text-[var(--qz-danger)]">
                      needs a tier-0 brick — the WAL lives there
                    </span>
                  )}
                </div>
                <div
                  className="rounded-md overflow-hidden"
                  style={{ border: "1px solid var(--qz-border)" }}
                >
                  {own.length === 0 && (
                    <div className="px-3 py-2 text-[12px] text-[var(--qz-fg-4)]">
                      No disks reported.
                    </div>
                  )}
                  {own.map((row) => {
                    const id = key(node, row.device.name);
                    // Something live has it: never offerable. A bare
                    // partition table is — the prepare wipes it through
                    // this member's own guards.
                    const disabled = row.device.claimed;
                    const selected = picked.has(id);
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
                          checked={selected}
                          disabled={disabled}
                          onChange={() =>
                            toggle(node, row.device.name, row.device.rotational)
                          }
                        />
                        <span className="qz-mono text-[12px] text-[var(--qz-fg-2)]">
                          {row.device.name}
                        </span>
                        <span className="text-[12px] text-[var(--qz-fg-4)]">
                          {formatBytes(row.device.size)}
                          {row.device.model && ` - ${row.device.model}`}
                          {row.device.rotational ? " - spinning" : " - solid state"}
                        </span>
                        {row.device.in_use && (
                          <span
                            className={`badge ml-auto ${disabled ? "badge-muted" : "badge-warn"}`}
                          >
                            {row.device.used_by ?? "in use"}
                          </span>
                        )}
                        {selected && (
                          <span className="ml-auto" style={{ width: 170 }}>
                            <SelectInput
                              value={String(picked.get(id) ?? 0)}
                              onChange={(value) =>
                                setPicked((current) => {
                                  const next = new Map(current);
                                  next.set(id, Number(value));
                                  return next;
                                })
                              }
                            >
                              {TIERS.map((tier) => (
                                <option key={tier.value} value={tier.value}>
                                  {tier.label}
                                </option>
                              ))}
                            </SelectInput>
                          </span>
                        )}
                      </label>
                    );
                  })}
                </div>
              </section>
            );
          })}
          <div className="flex justify-end">
            <button
              type="button"
              className="btn btn-primary btn-sm"
              disabled={!disksReady}
              onClick={() => setTab("review")}
            >
              Review
            </button>
          </div>
        </div>
      )}

      {tab === "review" && (
        <div className="flex flex-col gap-4">
          <table className="w-full text-[13px]">
            <tbody>
              {seats.map((seat) => (
                <tr key={seat.node} style={{ borderTop: "1px solid var(--qz-border)" }}>
                  <td className="py-2 pr-3 align-top qz-mono text-[12px]">
                    {shortNodeName(seat.node)}
                  </td>
                  <td className="py-2 text-[var(--qz-fg-3)]">
                    {seat.bricks.map((brick) => (
                      <div key={brick.disk} className="qz-mono text-[12px]">
                        {brick.disk} - tier {brick.tier}
                      </div>
                    ))}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {usableEstimate !== null && (
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              About <strong>{formatBytes(usableEstimate)} usable</strong>
              <span className="text-[var(--qz-fg-4)]">
                {" "}
                — the smaller member bounds each tier, and dedupe only makes
                this bigger, which is why raw is not quoted.
              </span>
            </div>
          )}
          <div className="text-[12px] text-[var(--qz-fg-4)]">
            Replication rides the cluster&apos;s Core network on port 7800; the
            machines&apos; disks then live on every member at once.
          </div>
          {submitError && <div className="callout callout-crit">{submitError}</div>}
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
            saving={submitting}
            disabled={!disksReady || !acked}
            submitLabel="Create the pool"
            savingLabel="Starting…"
            onSubmit={() => void submit()}
          />
        </div>
      )}

      {tab === "create" && progress && (
        <div className="flex flex-col gap-4">
          {progress.phase === "failed" && (
            <div className="callout callout-crit">
              {progress.error ?? "The create failed."}
            </div>
          )}
          <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
            {progress.steps.map((step, index) => (
              <ProgressRow key={`${step.step}-${step.node ?? ""}-${index}`} step={step} />
            ))}
          </ul>
          {adopting && !done && (
            <div className="text-[13px] text-[var(--qz-fg-3)]">
              The control plane is restarting to adopt the pool…
            </div>
          )}
          {done && (
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              The pool is up and answering on every member.
            </div>
          )}
          {(done || progress.phase === "failed") && (
            <div className="flex justify-end">
              <button type="button" className="btn btn-primary" onClick={onClose}>
                Close
              </button>
            </div>
          )}
        </div>
      )}
    </ModalShell>
  );
}

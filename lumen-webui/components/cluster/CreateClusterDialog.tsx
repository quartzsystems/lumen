"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Check, X } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { CoreBondPanel } from "@/components/cluster/CoreBondPanel";
import { Tabs } from "@/components/ui/Tabs";
import {
  CheckList,
  CheckRow,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
} from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  createCluster,
  fetchCreateProgress,
  linkLabel,
  preflightNodes,
  REGIME_LABEL,
  seatableLinks,
  type CreateProgress,
  type MemberCreate,
  type PreflightView,
  type StepProgress,
  type UnassignedNodeView,
} from "@/lib/clusterClient";

/// The create wizard, as a tabbed dialog like every other creator in this
/// console: every tab always reachable, invalid ones marked rather than
/// locked. The last tab is the create itself — per node, per step, unwind
/// included — because a wizard that closes on submit turns a five-minute
/// workflow into a spinner.
///
/// Fencing collects the BMC seats and passwords; the create writes the fence
/// devices into the CIB itself. The live per-direction fence tests are a
/// separate, deliberate act afterwards — from the Nodes page, one direction
/// at a time — because a test power-cycles a node.

interface MemberDraft {
  core_interface: string;
  core_address: string;
  management_interface: string;
  management_address: string;
  bmc_address: string;
  bmc_username: string;
  bmc_password: string;
}

const emptyDraft = (): MemberDraft => ({
  core_interface: "",
  core_address: "",
  management_interface: "",
  management_address: "",
  bmc_address: "",
  bmc_username: "ADMIN",
  bmc_password: "",
});

/// Propose the n-th host address inside a /24-ish subnet: base + n + 1.
const proposeAddress = (subnet: string, index: number): string => {
  const [base] = subnet.split("/");
  const parts = base?.split(".").map(Number) ?? [];
  if (parts.length !== 4 || parts.some(Number.isNaN)) return "";
  return `${parts[0]}.${parts[1]}.${parts[2]}.${parts[3] + index + 1}`;
};

export function CreateClusterDialog({
  unassigned,
  onClose,
  onCreated,
}: {
  unassigned: UnassignedNodeView[];
  onClose: () => void;
  onCreated: () => void;
}) {
  const [tab, setTab] = useState("members");
  const [error, setError] = useState<string | null>(null);

  // Members.
  const [name, setName] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [preflights, setPreflights] = useState<PreflightView[] | null>(null);
  const [preflightBusy, setPreflightBusy] = useState(false);
  const [preferred, setPreferred] = useState("");

  // Networks.
  const [coreSubnet, setCoreSubnet] = useState("10.10.0.0/24");
  const [coreMtu, setCoreMtu] = useState("9000");
  const [managementSubnet, setManagementSubnet] = useState("");
  const [vip, setVip] = useState("");
  const [drafts, setDrafts] = useState<Record<string, MemberDraft>>({});

  // Create.
  const [creating, setCreating] = useState(false);
  const [progress, setProgress] = useState<CreateProgress | null>(null);

  const toggle = (node: string) => {
    setPreflights(null);
    setSelected((current) =>
      current.includes(node) ? current.filter((n) => n !== node) : [...current, node],
    );
  };

  const draftOf = (node: string): MemberDraft => drafts[node] ?? emptyDraft();
  const patchDraft = (node: string, patch: Partial<MemberDraft>) =>
    setDrafts((current) => ({ ...current, [node]: { ...draftOf(node), ...patch } }));

  const runPreflight = async () => {
    setPreflightBusy(true);
    setError(null);
    try {
      const views = await preflightNodes(selected);
      setPreflights(views);
      // Seed what can be seeded: proposed Core addresses in the chosen
      // subnet, and each node's existing management addressing, adopted
      // rather than re-invented.
      setDrafts((current) => {
        const next = { ...current };
        views.forEach((view, index) => {
          const draft = next[view.node] ?? emptyDraft();
          if (!draft.core_address) {
            draft.core_address = proposeAddress(coreSubnet, index);
          }
          const managed = view.report?.links.find((link) => link.addresses.length > 0);
          if (!draft.management_interface && managed) {
            draft.management_interface = managed.name;
            draft.management_address = managed.addresses[0]?.split("/")[0] ?? "";
            const prefix = managed.addresses[0]?.split("/")[1];
            if (prefix && !managementSubnet) {
              const host = managed.addresses[0].split("/")[0].split(".");
              setManagementSubnet(`${host[0]}.${host[1]}.${host[2]}.0/${prefix}`);
            }
          }
          next[view.node] = draft;
        });
        return next;
      });
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Preflight failed.");
    } finally {
      setPreflightBusy(false);
    }
  };

  const request = useMemo(() => {
    const members: MemberCreate[] = selected.map((node) => {
      const draft = draftOf(node);
      return {
        node,
        core_interface: draft.core_interface,
        core_address: draft.core_address,
        management_interface: draft.management_interface,
        management_address: draft.management_address,
        bmc_address: draft.bmc_address,
        bmc_username: draft.bmc_username,
        bmc_password: draft.bmc_password,
      };
    });
    return {
      name: name.trim(),
      preferred_node: selected.length === 2 && preferred ? preferred : null,
      core: { subnet: coreSubnet.trim(), mtu: Number(coreMtu) || 9000 },
      management: {
        subnet: managementSubnet.trim(),
        vip: vip.trim() ? vip.trim() : null,
      },
      members,
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name, selected, preferred, coreSubnet, coreMtu, managementSubnet, vip, drafts]);

  const membersReady =
    name.trim().length > 0 &&
    selected.length >= 2 &&
    selected.length <= 5 &&
    preflights !== null &&
    preflights.every((view) => view.ok);
  const networksReady =
    coreSubnet.trim().length > 0 &&
    managementSubnet.trim().length > 0 &&
    selected.every((node) => {
      const draft = draftOf(node);
      return (
        draft.core_interface &&
        draft.core_address &&
        draft.management_interface &&
        draft.management_address &&
        draft.core_interface !== draft.management_interface
      );
    });
  const fencingReady = selected.every((node) => {
    const draft = draftOf(node);
    return draft.bmc_address.trim() && draft.bmc_username.trim() && draft.bmc_password.length > 0;
  });

  const create = async () => {
    setCreating(true);
    setError(null);
    try {
      const initial = await createCluster(request);
      setProgress(initial);
      setTab("progress");
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The create could not start.");
    } finally {
      setCreating(false);
    }
  };

  // While the create runs, its progress is the dialog.
  const running = progress?.phase === "running";
  const load = useCallback(async () => {
    try {
      setProgress(await fetchCreateProgress());
    } catch {
      // A dropped poll is retried on the next tick; the workflow keeps
      // running server-side either way.
    }
  }, []);
  useEffect(() => {
    if (!running) return;
    const timer = setInterval(() => void load(), 1000);
    return () => clearInterval(timer);
  }, [running, load]);
  useEffect(() => {
    if (progress?.phase === "complete") {
      onCreated();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [progress?.phase]);

  const tabs = [
    { value: "members", label: "Members", count: selected.length, invalid: tab !== "members" && !membersReady },
    { value: "networks", label: "Networks", invalid: tab !== "networks" && membersReady && !networksReady },
    { value: "fencing", label: "Fencing", invalid: tab !== "fencing" && membersReady && !fencingReady },
    { value: "review", label: "Review" },
    ...(progress ? [{ value: "progress", label: "Create" }] : []),
  ];

  return (
    <ModalShell onClose={running ? () => {} : onClose} maxWidth={680}>
      <ModalHeader
        title="Create Cluster"
        subtitle="2–5 nodes become one quorum, fencing, and replication domain."
        onClose={running ? () => {} : onClose}
      />
      <Tabs items={tabs} value={tab} onChange={setTab} className="mb-4" />

      {error && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}

      {tab === "members" && (
        <div className="flex flex-col gap-4">
          <Field label="Cluster name" htmlFor="cluster-name" required>
            <TextInput
              id="cluster-name"
              value={name}
              onChange={setName}
              mono
              autoFocus
              placeholder="alpha"
            />
          </Field>
          <Field
            label={`Members (${selected.length} of 2–5)`}
            hint="Unassigned environment nodes. A node joins the environment on the Nodes page first."
          >
            <CheckList>
              {unassigned.map((node) => (
                <CheckRow
                  key={node.node}
                  checked={selected.includes(node.node)}
                  onChange={() => toggle(node.node)}
                >
                  <span className="qz-mono text-[13px] text-[var(--qz-fg-1)]">{node.node}</span>
                  <span className="text-[12px] text-[var(--qz-fg-4)] ml-auto">
                    {node.address ?? ""}
                  </span>
                </CheckRow>
              ))}
              {unassigned.length === 0 && (
                <div className="text-[12px] text-[var(--qz-fg-4)] px-2 py-2">
                  Every environment node is already in a cluster.
                </div>
              )}
            </CheckList>
          </Field>

          <div className="flex items-center gap-3">
            <button
              type="button"
              className="btn btn-primary btn-sm"
              disabled={selected.length < 2 || preflightBusy}
              onClick={() => void runPreflight()}
            >
              {preflightBusy ? "Checking…" : "Run preflight"}
            </button>
            <span className="text-[12px] text-[var(--qz-fg-4)]">
              Version, clock, hostname, and existing cluster state — checked before anything is
              touched.
            </span>
          </div>

          {preflights !== null && (
            <ul className="m-0 p-0 flex flex-col gap-2" style={{ listStyle: "none" }}>
              {preflights.map((view) => (
                <li key={view.node} className="surface p-3 flex items-start gap-3">
                  {view.ok ? (
                    <Check size={16} className="text-[var(--qz-success)] mt-[2px]" />
                  ) : (
                    <X size={16} className="text-[var(--qz-danger)] mt-[2px]" />
                  )}
                  <div className="min-w-0">
                    <div className="qz-mono text-[13px] text-[var(--qz-fg-1)]">{view.node}</div>
                    {view.ok ? (
                      <div className="text-[12px] text-[var(--qz-fg-4)]">
                        Ready — {view.report?.links.length ?? 0} links reported.
                      </div>
                    ) : (
                      <ul className="m-0 mt-1 p-0 text-[12px] text-[var(--qz-fg-3)] flex flex-col gap-1" style={{ listStyle: "none" }}>
                        {view.problems.map((problem) => (
                          <li key={problem}>{problem}</li>
                        ))}
                      </ul>
                    )}
                  </div>
                </li>
              ))}
            </ul>
          )}

          {selected.length === 2 && (
            <Field
              label="Preferred node"
              hint="Two nodes race to fence each other in a partition where both are alive. The preferred node wins the race and survives."
            >
              <SelectInput value={preferred} onChange={setPreferred}>
                <option value="">Choose…</option>
                {selected.map((node) => (
                  <option key={node} value={node}>
                    {node}
                  </option>
                ))}
              </SelectInput>
            </Field>
          )}
        </div>
      )}

      {tab === "networks" && (
        <div className="flex flex-col gap-4">
          <div className="grid grid-cols-2 gap-3">
            <Field
              label="Core subnet"
              hint="Dedicated: replication and cluster heartbeat (ring 0). Routes nowhere."
              required
            >
              <TextInput value={coreSubnet} onChange={setCoreSubnet} mono placeholder="10.10.0.0/24" />
            </Field>
            <Field label="Core MTU" hint="Jumbo frames are the recommendation on a dedicated link.">
              <TextInput value={coreMtu} onChange={setCoreMtu} mono inputMode="numeric" />
            </Field>
            <Field
              label="Management subnet"
              hint="The console and ring 1 — usually the addressing the nodes already have."
              required
            >
              <TextInput
                value={managementSubnet}
                onChange={setManagementSubnet}
                mono
                placeholder="192.168.10.0/24"
              />
            </Field>
            <Field
              label="Cluster VIP (VIP)"
              hint="Optional: one stable console address that follows the survivors."
            >
              <TextInput value={vip} onChange={setVip} mono placeholder="192.168.10.100" />
            </Field>
          </div>

          {selected.map((node) => {
            const draft = draftOf(node);
            const links = seatableLinks(
              preflights?.find((v) => v.node === node)?.report?.links ?? [],
            );
            const sameLink =
              draft.core_interface !== "" && draft.core_interface === draft.management_interface;
            return (
              <section key={node} className="surface p-4 flex flex-col gap-3">
                <h3 className="qz-mono text-[13px] font-semibold text-[var(--qz-fg-1)] m-0">
                  {node}
                </h3>
                {sameLink && (
                  <div className="text-[12px] text-[var(--qz-danger)]">
                    Core and Management must not share a link — one cable would be both rings.
                  </div>
                )}
                <CoreBondPanel
                  node={node}
                  links={preflights?.find((v) => v.node === node)?.report?.links ?? []}
                  onBuilt={(bond) => {
                    // Seat Core on what was just built, then re-preflight so
                    // every picker sees the node's links as they now are.
                    patchDraft(node, { core_interface: bond });
                    void runPreflight();
                  }}
                />
                <div className="grid grid-cols-2 gap-3">
                  <Field label="Core NIC" required>
                    <SelectInput
                      mono
                      value={draft.core_interface}
                      onChange={(v) => patchDraft(node, { core_interface: v })}
                    >
                      <option value="">Choose…</option>
                      {links.map((link) => (
                        <option key={link.name} value={link.name}>
                          {linkLabel(link, "carrier")}
                        </option>
                      ))}
                    </SelectInput>
                  </Field>
                  <Field label="Core address" required>
                    <TextInput
                      mono
                      value={draft.core_address}
                      onChange={(v) => patchDraft(node, { core_address: v })}
                    />
                  </Field>
                  <Field label="Management NIC" required>
                    <SelectInput
                      mono
                      value={draft.management_interface}
                      onChange={(v) => patchDraft(node, { management_interface: v })}
                    >
                      <option value="">Choose…</option>
                      {links.map((link) => (
                        <option key={link.name} value={link.name}>
                          {linkLabel(link, "address")}
                        </option>
                      ))}
                    </SelectInput>
                  </Field>
                  <Field label="Management address" required>
                    <TextInput
                      mono
                      value={draft.management_address}
                      onChange={(v) => patchDraft(node, { management_address: v })}
                    />
                  </Field>
                </div>
              </section>
            );
          })}
          <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
            Ring 0 rides the Core network, ring 1 rides Management. Unaddressed Core NICs are
            given their address during the create; Management adopts what the node already has.
            To make Core survive a cable, build a bond on the node&rsquo;s Networking page first
            and pick it here — a bond is a Core seat like any other link.
          </p>
        </div>
      )}

      {tab === "fencing" && (
        <div className="flex flex-col gap-4">
          <div className="callout">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-fg-4)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-3)]">
              IPMI is the only fencing mechanism this appliance uses — one BMC per node, no
              fallback levels. The create writes one fence device per member into the cluster,
              password and all; the password goes nowhere else and is never shown back. Live
              fence tests run afterwards, from the Nodes page.
            </div>
          </div>
          {selected.map((node) => {
            const draft = draftOf(node);
            return (
              <section key={node} className="surface p-4 flex flex-col gap-3">
                <h3 className="qz-mono text-[13px] font-semibold text-[var(--qz-fg-1)] m-0">
                  {node}
                </h3>
                <div className="grid grid-cols-2 gap-3">
                  <Field label="BMC address" required hint="The BMC's own interface — it answers when the node cannot.">
                    <TextInput
                      mono
                      value={draft.bmc_address}
                      onChange={(v) => patchDraft(node, { bmc_address: v })}
                      placeholder="10.20.0.1"
                    />
                  </Field>
                  <Field label="BMC username" required>
                    <TextInput
                      mono
                      value={draft.bmc_username}
                      onChange={(v) => patchDraft(node, { bmc_username: v })}
                    />
                  </Field>
                  <Field
                    label="BMC password"
                    required
                    hint="Written into the cluster's fence device and kept nowhere else."
                  >
                    <TextInput
                      mono
                      type="password"
                      value={draft.bmc_password}
                      onChange={(v) => patchDraft(node, { bmc_password: v })}
                    />
                  </Field>
                </div>
              </section>
            );
          })}
        </div>
      )}

      {tab === "review" && (
        <div className="flex flex-col gap-4">
          <dl className="qz-facts m-0">
            <dt>Cluster</dt>
            <dd className="qz-mono">{name || "—"}</dd>
            <dt>Regime</dt>
            <dd>{REGIME_LABEL[selected.length === 2 ? "two_node" : "quorum"]}</dd>
            <dt>Members</dt>
            <dd className="qz-mono">{selected.join(", ") || "—"}</dd>
            {selected.length === 2 && (
              <>
                <dt>Preferred node</dt>
                <dd className="qz-mono">{preferred || "—"}</dd>
              </>
            )}
            <dt>Core</dt>
            <dd className="qz-mono">
              {coreSubnet} - MTU {coreMtu}
            </dd>
            <dt>Management</dt>
            <dd className="qz-mono">
              {managementSubnet || "—"}
              {vip ? ` - VIP ${vip}` : ""}
            </dd>
          </dl>
          <div className="surface p-4">
            <div className="text-[12px] text-[var(--qz-fg-4)] mb-2">
              Per-node seats — rings derive from these:
            </div>
            <table className="qz-table w-full">
              <thead>
                <tr>
                  <th>Node</th>
                  <th>ring0 (Core)</th>
                  <th>ring1 (Management)</th>
                  <th>BMC</th>
                </tr>
              </thead>
              <tbody>
                {selected.map((node) => {
                  const draft = draftOf(node);
                  return (
                    <tr key={node}>
                      <td className="mono">{node}</td>
                      <td className="mono">
                        {draft.core_interface} - {draft.core_address}
                      </td>
                      <td className="mono">
                        {draft.management_interface} - {draft.management_address}
                      </td>
                      <td className="mono">{draft.bmc_address}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <p className="text-[12px] text-[var(--qz-fg-4)] m-0">
            A failed create unwinds completely: configuration removed, addresses released, every
            node back to unassigned. The record is written only when the cluster has formed.
          </p>
          <ModalFooter
            onCancel={onClose}
            saving={creating}
            disabled={!membersReady || !networksReady || !fencingReady}
            submitLabel="Create cluster"
            savingLabel="Starting…"
            onSubmit={() => void create()}
          />
        </div>
      )}

      {tab === "progress" && progress && (
        <div className="flex flex-col gap-4">
          {progress.phase === "failed" && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {progress.error ?? "The create failed."} Every touched node was unwound.
              </div>
            </div>
          )}
          <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
            {progress.steps.map((step, index) => (
              <ProgressRow key={`${step.step}-${step.node ?? ""}-${index}`} step={step} />
            ))}
          </ul>
          {progress.phase !== "running" && (
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

export const STEP_LABEL: Record<string, string> = {
  preflight: "Preflight",
  generate: "Generate configuration",
  prepare: "Prepare",
  // The two steps only a node-add has: every existing member takes the new
  // membership live, and the two-node fence delays are flattened away.
  reconfigure: "Push new membership",
  delays: "Flatten fence delays",
  start: "Start cluster stack",
  form: "Wait for the cluster to form",
  properties: "Set cluster properties",
  fence: "Create fence device",
  record: "Record the cluster",
  unwind: "Unwind",
  // The pool workflows' steps ride the same renderer.
  restart: "Restart control plane",
  teardown: "Tear down",
  // The grow workflow's own: serving members take the newcomer's dial,
  // the map opens, the blocks move, and the commit makes it real.
  reconf: "Take the new member's link",
  reassign: "Open the new layout",
  rebalance: "Move blocks to the new member",
  commit: "Commit the new layout",
};

export function ProgressRow({ step }: { step: StepProgress }) {
  const tone =
    step.state === "done"
      ? "ok"
      : step.state === "failed"
        ? "crit"
        : step.state === "running"
          ? "warn"
          : "muted";
  return (
    <li className="flex items-start gap-2 text-[13px]">
      <span className={`state-dot-${tone} mt-[5px]`} />
      <span className="text-[var(--qz-fg-2)]">
        {STEP_LABEL[step.step] ?? step.step}
        {step.node && <span className="qz-mono text-[var(--qz-fg-3)]"> - {step.node}</span>}
      </span>
      <span className="text-[12px] text-[var(--qz-fg-4)] ml-auto text-right max-w-[300px]">
        {step.state === "unwound" ? "unwound" : (step.detail ?? step.state)}
      </span>
    </li>
  );
}

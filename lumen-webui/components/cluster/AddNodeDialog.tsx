"use client";

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Check, X } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  addClusterNode,
  fetchCreateProgress,
  linkLabel,
  preflightNodes,
  seatableLinks,
  type ClusterView,
  type CreateProgress,
  type PreflightView,
  type UnassignedNodeView,
} from "@/lib/clusterClient";
import { ProgressRow } from "@/components/cluster/CreateClusterDialog";

/// Add one node to a running cluster. One form, not a wizard — a single
/// newcomer has one set of seats. The consequential part happens server-side:
/// every existing member takes the grown membership live, and at 2→3 the
/// cluster leaves the two-node regime (delays flattened, wait_for_all gone,
/// majority quorum from now on). Existing volumes stay where they are; the
/// newcomer is new capacity, not an automatic replica.
export function AddNodeDialog({
  cluster,
  unassigned,
  onClose,
  onAdded,
}: {
  cluster: ClusterView;
  unassigned: UnassignedNodeView[];
  onClose: () => void;
  onAdded: () => void;
}) {
  const [node, setNode] = useState(unassigned[0]?.node ?? "");
  const [preflight, setPreflight] = useState<PreflightView | null>(null);
  const [preflightBusy, setPreflightBusy] = useState(false);
  const [coreInterface, setCoreInterface] = useState("");
  const [coreAddress, setCoreAddress] = useState("");
  const [managementInterface, setManagementInterface] = useState("");
  const [managementAddress, setManagementAddress] = useState("");
  const [bmcAddress, setBmcAddress] = useState("");
  const [bmcUsername, setBmcUsername] = useState("ADMIN");
  const [bmcPassword, setBmcPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<CreateProgress | null>(null);

  const runPreflight = async () => {
    setPreflightBusy(true);
    setError(null);
    try {
      const [view] = await preflightNodes([node]);
      setPreflight(view ?? null);
      // Adopt the node's existing management addressing rather than
      // re-inventing it — same seeding the create wizard does.
      const managed = view?.report?.links.find((link) => link.addresses.length > 0);
      if (managed && !managementInterface) {
        setManagementInterface(managed.name);
        setManagementAddress(managed.addresses[0]?.split("/")[0] ?? "");
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Preflight failed.");
    } finally {
      setPreflightBusy(false);
    }
  };

  const links = seatableLinks(preflight?.report?.links ?? []);
  const sameLink = coreInterface !== "" && coreInterface === managementInterface;
  const ready =
    node !== "" &&
    preflight?.ok === true &&
    coreInterface !== "" &&
    coreAddress.trim() !== "" &&
    managementInterface !== "" &&
    managementAddress.trim() !== "" &&
    !sameLink &&
    bmcAddress.trim() !== "" &&
    bmcUsername.trim() !== "" &&
    bmcPassword.length > 0;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const initial = await addClusterNode(cluster.name, {
        node,
        core_interface: coreInterface,
        core_address: coreAddress.trim(),
        management_interface: managementInterface,
        management_address: managementAddress.trim(),
        bmc_address: bmcAddress.trim(),
        bmc_username: bmcUsername.trim(),
        bmc_password: bmcPassword,
      });
      setProgress(initial);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The node-add could not start.");
    } finally {
      setBusy(false);
    }
  };

  // While the add runs, its progress is the dialog.
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
      onAdded();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [progress?.phase]);

  const leavingTwoNode = cluster.nodes.length === 2;

  return (
    <ModalShell onClose={running ? () => {} : onClose} maxWidth={620}>
      <ModalHeader
        title={`Add a node to ${cluster.name}`}
        subtitle={
          leavingTwoNode
            ? "2 → 3 nodes: the cluster leaves the two-node regime and runs on majority quorum from here on."
            : `${cluster.nodes.length} → ${cluster.nodes.length + 1} nodes.`
        }
        onClose={running ? () => {} : onClose}
      />

      {error && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}

      {progress ? (
        <div className="flex flex-col gap-4">
          {progress.phase === "failed" && (
            <div className="callout callout-crit">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {progress.error ?? "The node-add failed."} The newcomer was unwound and the
                cluster keeps its previous membership.
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
      ) : (
        <div className="flex flex-col gap-4">
          <Field
            label="Node"
            required
            hint="An unassigned environment node. A node joins the environment on the Nodes page first."
          >
            <SelectInput
              mono
              value={node}
              onChange={(v) => {
                setNode(v);
                setPreflight(null);
              }}
            >
              {unassigned.map((candidate) => (
                <option key={candidate.node} value={candidate.node}>
                  {candidate.node}
                  {candidate.address ? ` — ${candidate.address}` : ""}
                </option>
              ))}
            </SelectInput>
          </Field>

          <div className="flex items-center gap-3">
            <button
              type="button"
              className="btn btn-primary btn-sm"
              disabled={node === "" || preflightBusy}
              onClick={() => void runPreflight()}
            >
              {preflightBusy ? "Checking…" : "Run preflight"}
            </button>
            <span className="text-[12px] text-[var(--qz-fg-4)]">
              Version, clock, hostname, and existing cluster state — checked before anything is
              touched.
            </span>
          </div>

          {preflight !== null && (
            <div className="surface p-3 flex items-start gap-3">
              {preflight.ok ? (
                <Check size={16} className="text-[var(--qz-success)] mt-[2px]" />
              ) : (
                <X size={16} className="text-[var(--qz-danger)] mt-[2px]" />
              )}
              <div className="min-w-0">
                <div className="qz-mono text-[13px] text-[var(--qz-fg-1)]">{preflight.node}</div>
                {preflight.ok ? (
                  <div className="text-[12px] text-[var(--qz-fg-4)]">
                    Ready — {preflight.report?.links.length ?? 0} links reported.
                  </div>
                ) : (
                  <ul
                    className="m-0 mt-1 p-0 text-[12px] text-[var(--qz-fg-3)] flex flex-col gap-1"
                    style={{ listStyle: "none" }}
                  >
                    {preflight.problems.map((problem) => (
                      <li key={problem}>{problem}</li>
                    ))}
                  </ul>
                )}
              </div>
            </div>
          )}

          {preflight?.ok && (
            <>
              {sameLink && (
                <div className="text-[12px] text-[var(--qz-danger)]">
                  Core and Management must not share a link — one cable would be both rings.
                </div>
              )}
              <div className="grid grid-cols-2 gap-3">
                <Field label="Core NIC" required>
                  <SelectInput mono value={coreInterface} onChange={setCoreInterface}>
                    <option value="">Choose…</option>
                    {links.map((link) => (
                      <option key={link.name} value={link.name}>
                        {linkLabel(link, "carrier")}
                      </option>
                    ))}
                  </SelectInput>
                </Field>
                <Field
                  label="Core address"
                  required
                  hint="A free address in the cluster's Core subnet."
                >
                  <TextInput mono value={coreAddress} onChange={setCoreAddress} />
                </Field>
                <Field label="Management NIC" required>
                  <SelectInput mono value={managementInterface} onChange={setManagementInterface}>
                    <option value="">Choose…</option>
                    {links.map((link) => (
                      <option key={link.name} value={link.name}>
                        {linkLabel(link, "address")}
                      </option>
                    ))}
                  </SelectInput>
                </Field>
                <Field label="Management address" required>
                  <TextInput mono value={managementAddress} onChange={setManagementAddress} />
                </Field>
              </div>

              <div className="grid grid-cols-2 gap-3">
                <Field
                  label="BMC address"
                  required
                  hint="The BMC's own interface — it answers when the node cannot."
                >
                  <TextInput mono value={bmcAddress} onChange={setBmcAddress} placeholder="10.20.0.3" />
                </Field>
                <Field label="BMC username" required>
                  <TextInput mono value={bmcUsername} onChange={setBmcUsername} />
                </Field>
                <Field
                  label="BMC password"
                  required
                  hint="Written into the cluster's fence device and kept nowhere else. The new fence path is untested until you test it from the Nodes page."
                >
                  <TextInput mono type="password" value={bmcPassword} onChange={setBmcPassword} />
                </Field>
              </div>
            </>
          )}

          <ModalFooter
            onCancel={onClose}
            saving={busy}
            disabled={!ready}
            submitLabel="Add node"
            savingLabel="Starting…"
            onSubmit={() => void submit()}
          />
        </div>
      )}
    </ModalShell>
  );
}

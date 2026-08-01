"use client";

import { useCallback, useEffect, useState, type CSSProperties } from "react";
import { AlertTriangle, Check, Copy, X, Zap } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Tabs } from "@/components/ui/Tabs";
import { Field, ModalFooter, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  confirmNodeDead,
  destroyCluster,
  powerNode,
  enterMaintenance,
  exitMaintenance,
  fetchDrain,
  joinEnvironment,
  mintToken,
  testFence,
  type ClusterNodeView,
  type ClusterView,
  type NodePowerAction,
  type FenceTestView,
  type MaintenanceProgress,
} from "@/lib/clusterClient";

/// Both directions of the join-token flow in one dialog: mint a token here
/// for another node to paste, or paste a token minted elsewhere. One string
/// carries the whole handshake — where to call, the one-time secret, and the
/// fingerprint of the certificate the issuer will present.
export function AddNodeDialog({
  hasEnvironment,
  onClose,
}: {
  hasEnvironment: boolean;
  onClose: () => void;
}) {
  const [tab, setTab] = useState(hasEnvironment ? "mint" : "join");
  const [error, setError] = useState<string | null>(null);

  // Mint.
  const [minted, setMinted] = useState<string | null>(null);
  const [minting, setMinting] = useState(false);
  const [copied, setCopied] = useState(false);

  // Join.
  const [token, setToken] = useState("");
  const [joining, setJoining] = useState(false);
  const [joinedNote, setJoinedNote] = useState<string | null>(null);

  const mint = async () => {
    setMinting(true);
    setError(null);
    try {
      const answer = await mintToken();
      setMinted(answer.token);
      setCopied(false);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not mint a token.");
    } finally {
      setMinting(false);
    }
  };

  const join = async () => {
    setJoining(true);
    setError(null);
    try {
      const answer = await joinEnvironment(token.trim());
      setJoinedNote(answer.note);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The join failed.");
    } finally {
      setJoining(false);
    }
  };

  return (
    <ModalShell onClose={onClose} maxWidth={560}>
      <ModalHeader
        title="Add Node"
        subtitle="A node joins the environment with a one-time token carried between consoles."
        onClose={onClose}
      />
      <Tabs
        items={[
          { value: "mint", label: "Mint a token here" },
          { value: "join", label: "Join with a token" },
        ]}
        value={tab}
        onChange={setTab}
        className="mb-4"
      />

      {error && (
        <div className="callout callout-crit mb-4">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
        </div>
      )}

      {tab === "mint" && (
        <div className="flex flex-col gap-4">
          <p className="text-[13px] text-[var(--qz-fg-3)] m-0">
            Mint a token on this node, then paste it into the other node&apos;s console under
            Infrastructure → Nodes → Add Node. Tokens are one-time and expire in 15 minutes.
            {!hasEnvironment &&
              " Minting the first token creates the environment, with this node as its first member."}
          </p>
          {minted === null ? (
            <div>
              <button
                type="button"
                className="btn btn-primary"
                disabled={minting}
                onClick={() => void mint()}
              >
                {minting ? "Minting…" : "Mint token"}
              </button>
            </div>
          ) : (
            <div className="flex flex-col gap-2">
              <textarea
                readOnly
                value={minted}
                rows={4}
                className="w-full rounded-md px-3 py-[9px] text-[12px] text-[var(--qz-fg-1)] outline-none resize-none"
                style={{
                  background: "var(--qz-input-bg)",
                  border: "1px solid var(--qz-border)",
                  fontFamily: "var(--qz-font-mono)",
                  wordBreak: "break-all",
                }}
                onFocus={(e) => e.currentTarget.select()}
              />
              <div className="flex items-center gap-3">
                <button
                  type="button"
                  className="btn btn-ghost btn-sm"
                  onClick={() => {
                    void navigator.clipboard.writeText(minted);
                    setCopied(true);
                  }}
                >
                  <Copy size={14} /> {copied ? "Copied" : "Copy"}
                </button>
                <span className="text-[12px] text-[var(--qz-fg-4)]">
                  Anyone holding this string can join a node to this environment until it
                  expires. Treat it accordingly.
                </span>
              </div>
            </div>
          )}
        </div>
      )}

      {tab === "join" &&
        (joinedNote ? (
          <div className="flex flex-col gap-4">
            <div className="callout">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">{joinedNote}</div>
            </div>
            <div className="flex justify-end">
              <button
                type="button"
                className="btn btn-primary"
                onClick={() => window.location.assign("/login")}
              >
                Go to sign-in
              </button>
            </div>
          </div>
        ) : (
          <div className="flex flex-col gap-4">
            <Field
              label="Join token"
              hint="Paste the whole string minted on an environment node. Joining replaces this node's sessions with the environment's — you will sign in again, and that is the join working."
            >
              <textarea
                value={token}
                onChange={(e) => setToken(e.target.value)}
                rows={4}
                placeholder="lumen-join/v1/…"
                className="w-full rounded-md px-3 py-[9px] text-[12px] text-[var(--qz-fg-1)] outline-none resize-none"
                style={{
                  background: "var(--qz-input-bg)",
                  border: "1px solid var(--qz-border)",
                  fontFamily: "var(--qz-font-mono)",
                  wordBreak: "break-all",
                }}
              />
            </Field>
            {hasEnvironment && (
              <div className="text-[12px] text-[var(--qz-danger)]">
                This node is already in an environment — a node belongs to exactly one.
              </div>
            )}
            <ModalFooter
              onCancel={onClose}
              saving={joining}
              disabled={!token.trim() || hasEnvironment}
              submitLabel="Join environment"
              savingLabel="Joining…"
              onSubmit={() => void join()}
            />
          </div>
        ))}
    </ModalShell>
  );
}

/// A guarded live fence test: the target node really power-cycles through
/// its BMC, because an untested fence path is one that fails during the
/// outage that needed it. The dialog says exactly what will happen, takes a
/// checkbox, and then shows the answer — pass or fail — rather than closing.
export function FenceTestDialog({
  cluster,
  node,
  isLocal,
  onClose,
  onTested,
}: {
  cluster: string;
  node: string;
  /// The target is the node this console is signed into — the one direction
  /// this console cannot test, and the dialog says so instead of trying.
  isLocal: boolean;
  onClose: () => void;
  onTested: () => void;
}) {
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<FenceTestView | null>(null);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      const answer = await testFence(cluster, node);
      setOutcome(answer);
      onTested();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The fence test could not run.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title={`Test fencing of ${node}`}
        subtitle={`Prove the cluster can actually kill ${node} when it must.`}
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        {outcome ? (
          <>
            <div className={`callout ${outcome.passed ? "" : "callout-crit"}`}>
              {outcome.passed ? (
                <Check size={17} className="flex-shrink-0 text-[var(--qz-success)] mt-[1px]" />
              ) : (
                <X size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
              )}
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {outcome.passed
                  ? `${node} was fenced. It is power-cycling now and will rejoin the cluster on its own — the direction is proven and recorded.`
                  : `The fence path to ${node} does not work: ${outcome.error ?? "the fence operation failed"}. Recorded — fix the BMC before an outage needs this path.`}
              </div>
            </div>
            <div className="flex justify-end">
              <button type="button" className="btn btn-primary" onClick={onClose}>
                Close
              </button>
            </div>
          </>
        ) : isLocal ? (
          <div className="callout">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This console runs on {node}, and a node does not run the test that powers itself
              off — the answer would go down with it. Open another member&apos;s console and
              test this direction from there.
            </div>
          </div>
        ) : (
          <>
            <div className="callout callout-warn">
              <Zap size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                {node} will be powered off through its BMC and boot back up. Machines running
                on it stop. The test takes a minute either way, and the result — pass or fail —
                is recorded against this direction.
              </div>
            </div>
            <label className="qz-check-row">
              <input
                type="checkbox"
                className="qz-check"
                checked={acked}
                onChange={() => setAcked(!acked)}
                style={{ "--qz-check-accent": "var(--qz-warn)" } as CSSProperties}
              />
              <span className="text-[13px] text-[var(--qz-fg-2)]">
                I understand this power-cycles {node}.
              </span>
            </label>
            <ModalFooter
              onCancel={onClose}
              saving={busy}
              disabled={!acked}
              submitLabel="Run fence test"
              savingLabel="Fencing…"
              onSubmit={() => void run()}
            />
          </>
        )}
      </div>
    </ModalShell>
  );
}

/// How often the drain feed is re-read while machines are moving. A live
/// migration takes as long as the machine's memory takes to copy, so this is
/// about watching progress, not about catching a fast transition.
const DRAIN_POLL_MS = 1500;

/// Taking this node out of service, and watching it empty.
///
/// One dialog for both directions, because they are one decision an operator
/// makes twice: out for the work, back afterwards. The drain's per-machine
/// steps are shown as they happen — a live migration is slow enough that a
/// spinner with no detail is worse than useless — and a machine that could
/// not move is named with its reason rather than folded into a count.
export function MaintenanceDialog({
  cluster,
  node,
  inMaintenance,
  onClose,
  onChanged,
}: {
  cluster: string;
  node: string;
  /// Whether the node is already out of service, which is what decides
  /// whether this dialog offers to leave or to return.
  inMaintenance: boolean;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [evacuate, setEvacuate] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<MaintenanceProgress | null>(null);

  // Pick up a drain that is already running — the operator may have closed
  // this dialog and come back, and the work does not stop when they do.
  const poll = useCallback(async () => {
    try {
      const latest = await fetchDrain();
      if (latest && latest.node === node) setProgress(latest);
    } catch {
      // A feed that cannot be read is not worth an error banner while the
      // real work is reported by the request itself.
    }
  }, [node]);

  useEffect(() => {
    void poll();
  }, [poll]);

  const draining = progress?.phase === "running";
  useEffect(() => {
    if (!draining) return;
    const timer = setInterval(() => void poll(), DRAIN_POLL_MS);
    return () => clearInterval(timer);
  }, [draining, poll]);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      if (inMaintenance) {
        await exitMaintenance(cluster, node);
        onChanged();
        onClose();
        return;
      }
      setProgress(await enterMaintenance(cluster, node, evacuate));
      onChanged();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The request was refused.");
    } finally {
      setBusy(false);
    }
  };

  const stranded = progress?.stranded ?? [];
  const finished = progress !== null && progress.phase !== "running";

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title={inMaintenance ? `Return ${node} to service` : `Take ${node} out of service`}
        subtitle={
          inMaintenance
            ? "Pacemaker runs things here again, and the cluster stops expecting this node to be away."
            : "Move the running machines off, and tell the cluster this absence is deliberate."
        }
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        {progress && <DrainSteps progress={progress} />}

        {finished && stranded.length > 0 && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="flex flex-col gap-2 text-[13px] text-[var(--qz-fg-2)]">
              <span>
                {node} is out of service, but {stranded.length}{" "}
                {stranded.length === 1 ? "machine is" : "machines are"} still running on it.
                Restarting the node now stops {stranded.length === 1 ? "it" : "them"}.
              </span>
              <ul className="m-0 pl-4 flex flex-col gap-1">
                {stranded.map((machine) => (
                  <li key={machine.vmid}>
                    <span className="qz-mono">{machine.name}</span> — {machine.reason}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        )}

        {finished && stranded.length === 0 && (
          <div className="callout">
            <Check size={17} className="flex-shrink-0 text-[var(--qz-success)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              {node} is empty and out of service. Restart or shut it down whenever you are ready;
              nothing here will be restarted elsewhere while it is away.
            </div>
          </div>
        )}

        {!progress && !inMaintenance && (
          <>
            <div className="callout">
              <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
              <div className="text-[13px] text-[var(--qz-fg-2)]">
                The cluster VIP moves off {node} straight away, so work on this node through its
                own address rather than the VIP. Machines that move stay where they went — there
                is no automatic move back.
              </div>
            </div>
            <label className="qz-check-row">
              <input
                type="checkbox"
                className="qz-check"
                checked={evacuate}
                onChange={() => setEvacuate(!evacuate)}
              />
              <span className="text-[13px] text-[var(--qz-fg-2)]">
                Move the running machines to other members first.
              </span>
            </label>
          </>
        )}

        {finished || (inMaintenance && progress) ? (
          <div className="flex justify-end">
            <button type="button" className="btn btn-primary" onClick={onClose}>
              Close
            </button>
          </div>
        ) : (
          <ModalFooter
            onCancel={onClose}
            saving={busy || draining}
            disabled={draining}
            submitLabel={inMaintenance ? "Return to service" : "Take out of service"}
            savingLabel={inMaintenance ? "Returning…" : "Draining…"}
            onSubmit={() => void run()}
          />
        )}
      </div>
    </ModalShell>
  );
}

/// The drain's steps, one line each: the node leaving service, then every
/// machine that has to move and where it went.
function DrainSteps({ progress }: { progress: MaintenanceProgress }) {
  const tone = (state: string): string =>
    state === "done" ? "ok" : state === "failed" ? "crit" : state === "running" ? "warn" : "muted";
  return (
    <ul className="m-0 p-0 flex flex-col gap-[6px]" style={{ listStyle: "none" }}>
      {progress.steps.map((step) => (
        <li key={step.step} className="flex items-center gap-2 text-[13px]">
          <span className={`state-dot-${tone(step.state)}`} />
          <span className="qz-mono text-[var(--qz-fg-2)]">{step.step}</span>
          {step.node && (
            <span className="text-[12px] text-[var(--qz-fg-4)]">→ {step.node}</span>
          )}
          {step.detail && (
            <span className="text-[12px] text-[var(--qz-fg-4)] ml-auto text-right">
              {step.detail}
            </span>
          )}
        </li>
      ))}
    </ul>
  );
}

/// Cut or cycle a member's power through its fence device.
///
/// The console's own power page asks the node's operating system, which is
/// exactly what cannot be relied on when a node needs this — a wedged
/// kernel, a refusing logind, a machine that stopped answering. This asks
/// the BMC instead, through the fence device the cluster already holds
/// credentials for, so it works when nothing on the target does.
///
/// Typed confirmation and a checkbox, the same as the break-glass dialog:
/// nothing is shut down cleanly here, and a machine still running on the
/// target stops where it stands.
export function NodePowerDialog({
  node,
  action,
  onClose,
  onDone,
}: {
  node: ClusterNodeView;
  action: NodePowerAction;
  onClose: () => void;
  onDone: (message: string) => void;
}) {
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmed = typed.trim() === node.node && acked;
  const cutting = action === "off";

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      await powerNode(node.node, action);
      onDone(
        cutting
          ? `${node.node} was powered off at its BMC.`
          : `${node.node} was power-cycled at its BMC — it comes back on its own.`,
      );
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The power command was refused.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={cutting ? `Power off ${node.node}?` : `Power-cycle ${node.node}?`}
        subtitle="Through its fence device, at the BMC — the node itself is not asked."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            This takes the power away at the machine. Every virtual machine on {node.node} stops
            where it is, with no shutdown — the same thing the cluster does to a node it has to
            fence.{" "}
            {cutting
              ? "It stays off until somebody turns it back on at its BMC or in the rack; this console cannot."
              : "It comes back on by itself and boots normally."}
          </div>
        </div>
        {!node.fence && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              This node has no fence device, so there is no power path to it. The command will be
              refused.
            </div>
          </div>
        )}
        <div className="text-[13px] text-[var(--qz-fg-3)]">
          To stop a node <em>gracefully</em> — machines moved off first — take it through
          Maintenance instead. This is the path for a node that is not answering.
        </div>
        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand the machines on {node.node} stop without shutting down.
          </span>
        </label>
        <Field label={`Type ${node.node} to confirm`} htmlFor="power-node-name">
          <TextInput
            id="power-node-name"
            value={typed}
            onChange={setTyped}
            mono
            autoFocus
            placeholder={node.node}
          />
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          submitLabel={cutting ? "Power off" : "Power-cycle"}
          savingLabel="Sending…"
          onSubmit={() => void run()}
        />
      </div>
    </ModalShell>
  );
}

/// The break-glass: only offered for a node that is unreachable and could
/// not be fenced. The operator types the node's name to vouch for a fact
/// the cluster cannot verify — that the machine is really powered off — and
/// the consequence of vouching wrongly is written out in full.
export function ConfirmDeadDialog({
  cluster,
  node,
  onClose,
  onConfirmed,
}: {
  cluster: string;
  node: string;
  onClose: () => void;
  onConfirmed: () => void;
}) {
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmed = typed.trim() === node && acked;

  const confirm = async () => {
    setBusy(true);
    setError(null);
    try {
      await confirmNodeDead(cluster, node);
      onConfirmed();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The confirmation was refused.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Confirm ${node} is dead`}
        subtitle="The cluster could not fence this node, so it is waiting on you instead."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <div className="callout callout-crit">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            Confirming makes the cluster recover as if fencing succeeded. If {node} is in fact
            still running, both sides will write the same volumes and that data will not
            survive. Verify the power is off at the machine — lights, power switch, the BMC of
            a different node — not from this console.
          </div>
        </div>
        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I have personally verified {node} is powered off.
          </span>
        </label>
        <Field label={`Type ${node} to confirm`} htmlFor="confirm-dead-name">
          <TextInput
            id="confirm-dead-name"
            value={typed}
            onChange={setTyped}
            mono
            autoFocus
            placeholder={node}
          />
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          submitLabel="Confirm dead"
          savingLabel="Confirming…"
          onSubmit={() => void confirm()}
        />
      </div>
    </ModalShell>
  );
}

/// Typed-name confirmation, exactly like destroying a pool: a checkbox is
/// ticked without reading, and typing the name cannot be done to the wrong
/// cluster by accident — which is the mistake that actually happens.
export function DestroyClusterDialog({
  cluster,
  onClose,
  onDestroyed,
}: {
  cluster: ClusterView;
  onClose: () => void;
  onDestroyed: () => void;
}) {
  const [typed, setTyped] = useState("");
  const [acked, setAcked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const confirmed = typed.trim() === cluster.name && acked;

  const destroy = async () => {
    setBusy(true);
    setError(null);
    try {
      await destroyCluster(cluster.name, true);
      onDestroyed();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "Could not destroy the cluster.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={onClose}>
      <ModalHeader
        title={`Destroy ${cluster.name}`}
        subtitle="The cluster stack stops on every member and its configuration is removed. The nodes return to unassigned."
        onClose={onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}
        <label className="qz-check-row">
          <input
            type="checkbox"
            className="qz-check"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ "--qz-check-accent": "var(--qz-danger)" } as CSSProperties}
          />
          <span className="text-[13px] text-[var(--qz-fg-2)]">
            I understand this stops the cluster on {cluster.nodes.length} node
            {cluster.nodes.length === 1 ? "" : "s"}.
          </span>
        </label>
        <Field label={`Type ${cluster.name} to confirm`} htmlFor="destroy-cluster-name">
          <TextInput
            id="destroy-cluster-name"
            value={typed}
            onChange={setTyped}
            mono
            autoFocus
            placeholder={cluster.name}
          />
        </Field>
        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!confirmed}
          submitLabel="Destroy cluster"
          savingLabel="Destroying…"
          onSubmit={() => void destroy()}
        />
      </div>
    </ModalShell>
  );
}

"use client";

import { useState } from "react";
import { AlertTriangle, Copy } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Tabs } from "@/components/ui/Tabs";
import { Field, ModalFooter, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import {
  joinEnvironment,
  mintToken,
  type ClusterView,
  destroyCluster,
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
        <label className="qz-check">
          <input
            type="checkbox"
            checked={acked}
            onChange={() => setAcked(!acked)}
            style={{ accentColor: "var(--qz-danger)" }}
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

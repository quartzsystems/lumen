"use client";

import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { updateCoreNetwork, type ClusterNetworks } from "@/lib/clusterClient";
import type { InventoryResponse } from "@/lib/inventoryClient";
import { shortNodeName } from "@/lib/nodeNames";

/// Change the Core network without destroying the cluster: the MTU, and
/// which link carries each member's seat.
///
/// The subnet and the addresses are shown and not offered. They are
/// corosync's ring addressing — the ring's identity, written into every
/// member's configuration and ridden by the pool's peer links — and a form
/// that cannot carry them is a form that cannot quietly ask for a
/// renumbering. Moving a seat to another link, or changing the frame size,
/// touches none of that: each member re-realizes the same seat through its
/// own networking domain, inside its own checkpoint.
export function EditCoreNetworkDialog({
  cluster,
  networks,
  inventory,
  onClose,
  onSaved,
}: {
  cluster: string;
  networks: ClusterNetworks;
  /// Every member's links, for the seat pickers. Null when the environment
  /// read failed — the pickers then keep the recorded interface and the MTU
  /// stays editable.
  inventory: InventoryResponse | null;
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const core = networks.core;
  const [mtu, setMtu] = useState(String(core.mtu));
  const [seats, setSeats] = useState<Record<string, string>>(() =>
    Object.fromEntries(core.members.map((member) => [member.node, member.interface])),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  /// The links a member's seat could move to: what that member reports
  /// having, minus what cannot carry a seat — an unmanaged link, a port
  /// already enslaved to a controller, or the link carrying that member's
  /// Management seat (the rings must not share a cable). The backend
  /// re-checks all of it against a fresh report; this list is so the picker
  /// offers what will pass.
  const candidates = (node: string): { name: string; note: string }[] => {
    const links = inventory?.members.find((member) => member.node === node)?.inventory
      ?.interfaces;
    const management = networks.management.members.find(
      (member) => member.node === node,
    )?.interface;
    const recorded = seats[node];
    if (!links) {
      return recorded ? [{ name: recorded, note: "as recorded" }] : [];
    }
    return links
      .filter(
        (link) =>
          link.kind !== "other" &&
          link.name !== management &&
          (link.controller === null || link.name === recorded),
      )
      .map((link) => ({
        name: link.name,
        note: `${link.kind}${link.carrier ? "" : " - no carrier"}`,
      }));
  };

  const wantedMtu = Number(mtu.trim());
  const badMtu = !Number.isInteger(wantedMtu) || wantedMtu < 576 || wantedMtu > 9216;
  const moved = core.members.filter((member) => seats[member.node] !== member.interface);
  const changed = (!badMtu && wantedMtu !== core.mtu) || moved.length > 0;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await updateCoreNetwork(cluster, {
        mtu: wantedMtu !== core.mtu ? wantedMtu : undefined,
        members:
          moved.length > 0
            ? core.members.map((member) => ({
                node: member.node,
                interface: seats[member.node] ?? member.interface,
                // The addresses travel exactly as recorded — the request
                // shape carries them so the backend can prove nothing moved.
                address: member.address,
              }))
            : undefined,
      });
      onSaved(
        moved.length > 0
          ? `The Core network changed. ${moved
              .map((member) => shortNodeName(member.node))
              .join(", ")} now carr${moved.length === 1 ? "ies" : "y"} it on the new link.`
          : `The Core MTU is now ${wantedMtu} on every member.`,
      );
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(err instanceof Error ? err.message : "The Core network could not be changed.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <ModalShell onClose={busy ? () => {} : onClose} maxWidth={560}>
      <ModalHeader
        title="Core network"
        subtitle={`${cluster}'s replication and heartbeat network: subnet ${core.subnet}.`}
        onClose={busy ? () => {} : onClose}
      />
      <div className="flex flex-col gap-4">
        {error && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">{error}</div>
          </div>
        )}

        <Field
          label="MTU"
          htmlFor="core-mtu"
          hint="Jumbo frames (9000) are the usual choice on a dedicated replication link."
          error={badMtu ? "Use an MTU from 576 to 9216." : undefined}
        >
          <TextInput
            id="core-mtu"
            value={mtu}
            mono
            inputMode="numeric"
            invalid={badMtu}
            onChange={setMtu}
          />
        </Field>

        {core.members.map((member) => (
          <Field
            key={member.node}
            label={`Seat on ${shortNodeName(member.node)}`}
            htmlFor={`core-seat-${member.node}`}
            hint={
              <>
                Keeps its address <span className="qz-mono">{member.address}</span> — the
                ring&apos;s name for this member does not change, only the link carrying it.
              </>
            }
          >
            <SelectInput
              id={`core-seat-${member.node}`}
              value={seats[member.node] ?? member.interface}
              mono
              onChange={(value) => setSeats((s) => ({ ...s, [member.node]: value }))}
            >
              {candidates(member.node).map((candidate) => (
                <option key={candidate.name} value={candidate.name}>
                  {candidate.name} ({candidate.note})
                </option>
              ))}
            </SelectInput>
          </Field>
        ))}

        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            Members change one at a time, each inside its own checkpoint. A moved seat drops that
            member&apos;s replication link for a moment while the address crosses to the new
            cable; the Management ring keeps the cluster together through it. If a member fails
            partway, the error names it and the record keeps the old definition — fix that member
            and apply again.
          </div>
        </div>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={busy || badMtu || !changed}
          submitLabel="Apply to every member"
          savingLabel="Applying…"
          onSubmit={() => void submit()}
        />
      </div>
    </ModalShell>
  );
}

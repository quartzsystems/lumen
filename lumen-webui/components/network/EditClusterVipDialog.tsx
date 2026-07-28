"use client";

import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Field, ModalFooter, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { setClusterVip, type ClusterNetworks, type ClusterView } from "@/lib/clusterClient";
import { shortNodeName } from "@/lib/nodeNames";

/// Move the cluster VIP, or take it away.
///
/// The dialog exists to make one thing unmissable before it happens: the old
/// address comes down before the new one goes up, so a console reached on the
/// VIP loses its connection in the middle. That is not a failure — the change
/// completes on the cluster regardless of whether anyone is still listening,
/// and every member's own address stays valid throughout, which is what makes
/// it something to warn about rather than refuse.
///
/// Clearing the field is how the address is removed. There is no separate
/// delete control, because "no cluster VIP" is a value this field can
/// hold, and two ways to say the same thing is one more than an operator
/// should have to choose between.
export function EditClusterVipDialog({
  cluster,
  networks,
  onClose,
  onSaved,
}: {
  cluster: ClusterView | null;
  /// The cluster's networks, for the Management subnet the address has to
  /// live in and the member addresses it must not collide with. Null when
  /// the record could not be read — the dialog then leaves the checking to
  /// the backend rather than guessing.
  networks: ClusterNetworks | null;
  onClose: () => void;
  onSaved: (message: string) => void;
}) {
  const current = networks?.management.vip ?? null;
  const [address, setAddress] = useState(current ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const wanted = address.trim();
  const removing = wanted === "";
  const malformed = !removing && !/^\d{1,3}(\.\d{1,3}){3}$/.test(wanted);

  // The two checks the record can answer locally. Both are refused by the
  // backend too — this is so the operator is told before they submit, not
  // after the address has already been taken down.
  const taken = networks?.management.members.find((member) => member.address === wanted);
  const outside = !removing && !malformed && networks !== null && !inSubnet(wanted, networks);

  const unchanged = (current ?? "") === wanted;
  const ready = !busy && !unchanged && !malformed && !taken && !outside;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await setClusterVip(cluster?.name ?? "", removing ? null : wanted);
      onSaved(
        removing
          ? `${cluster?.name} no longer has a cluster VIP. Each member is still reachable on its own.`
          : `The cluster VIP is now ${wanted}. If this console was reached on the old one, open the new one — or any member's own address.`,
      );
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      // A dropped connection here is the operation working, not failing: this
      // console was talking to the address that just came down. Said plainly,
      // because a red error over a change that succeeded is worse than none.
      if (err instanceof ApiError && err.status === 0) {
        onSaved(
          `The connection dropped, which is what changing the address this console was reached on does. Open ${wanted || "a member's own address"} to carry on.`,
        );
        return;
      }
      setError(err instanceof Error ? err.message : "The cluster VIP could not be changed.");
    } finally {
      setBusy(false);
    }
  };

  const holder = cluster?.vip?.state?.node;

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title="Cluster VIP"
        subtitle={`One address for ${cluster?.name ?? "the cluster"}'s console that follows the surviving members.`}
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
          label="Address"
          htmlFor="vip-address"
          hint={
            networks
              ? `Inside the Management subnet ${networks.management.subnet}, and an address no member already holds. Leave it empty to remove the cluster VIP.`
              : "Leave it empty to remove the cluster VIP."
          }
          error={
            malformed
              ? "That is not an IPv4 address."
              : taken
                ? `${wanted} is already ${shortNodeName(taken.node)}'s own address. The cluster VIP moves between members, so it has to be one nothing else holds.`
                : outside
                  ? `${wanted} is outside the Management subnet ${networks?.management.subnet} — nothing would route to it.`
                  : undefined
          }
        >
          <TextInput
            id="vip-address"
            value={address}
            onChange={setAddress}
            placeholder="172.16.20.70"
            mono
            autoFocus
            invalid={malformed || taken !== undefined || outside}
          />
        </Field>

        <div className="callout callout-warn">
          <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
          <div className="text-[13px] text-[var(--qz-fg-2)]">
            {removing ? (
              <>
                The address comes down and is not replaced.{" "}
                {holder && (
                  <>
                    It is on <span className="qz-mono">{shortNodeName(holder)}</span> now.{" "}
                  </>
                )}
                Anything pointed at it — console bookmarks most of all — stops answering. Each
                member is still reachable on its own address.
              </>
            ) : (
              <>
                The old address comes down before the new one goes up. If this console is reached
                on it, the connection drops mid-change — the change still completes, and every
                member&apos;s own address keeps working throughout.
              </>
            )}
          </div>
        </div>

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!ready}
          submitLabel={removing ? "Remove address" : "Change address"}
          savingLabel="Applying…"
          onSubmit={() => void submit()}
        />
      </div>
    </ModalShell>
  );
}

/// Whether an address falls inside the Management subnet, by the same
/// arithmetic the backend uses — masked network parts compared, nothing else.
///
/// Best-effort on purpose: a subnet this cannot parse means no complaint here
/// and the backend's own answer stands. A dialog that refused an address it
/// merely failed to understand would be worse than one that lets the cluster
/// say so.
function inSubnet(address: string, networks: ClusterNetworks): boolean {
  const [base, bits] = networks.management.subnet.split("/");
  const prefix = Number(bits);
  const toInt = (value: string): number | null => {
    const parts = value.split(".").map(Number);
    if (parts.length !== 4 || parts.some((part) => !Number.isInteger(part) || part < 0 || part > 255))
      return null;
    // Unsigned: the shift below makes the top octet negative otherwise, and
    // two negatives compare fine but a mixed pair does not.
    return ((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3]) >>> 0;
  };
  const network = toInt(base);
  const candidate = toInt(address);
  if (network === null || candidate === null || !Number.isInteger(prefix) || prefix < 0 || prefix > 32)
    return true;
  if (prefix === 0) return true;
  const mask = (0xffffffff << (32 - prefix)) >>> 0;
  return (network & mask) === (candidate & mask);
}

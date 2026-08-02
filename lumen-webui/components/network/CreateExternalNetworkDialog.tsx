"use client";

import { useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import {
  CheckList,
  CheckRow,
  Field,
  ModalFooter,
  SelectInput,
  TextInput,
} from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { shortNodeName, shortNodeNames } from "@/lib/nodeNames";
import {
  bondNameFits,
  bondNameFor,
  createExternalNetwork,
  updateExternalNetwork,
  type ClusterView,
  type ExternalNetwork,
  type ExternalNetworkCreate,
  type NetworkType,
  type Uplink,
} from "@/lib/clusterClient";
import { linksByMember, type InventoryResponse } from "@/lib/inventoryClient";
import type { BondMode } from "@/lib/networkClient";

/// An External network being changed, with the cluster it belongs to. The
/// table this is opened from spans clusters, so a name alone does not
/// identify one.
export interface ExternalNetworkEdit {
  cluster: string;
  network: ExternalNetwork;
}

/// The kinds of network, and whether this appliance can build one yet.
///
/// The unbuildable two are listed rather than hidden. An operator deciding
/// what to put VM traffic on is deciding between three things whether or not
/// the console says so, and a picker that offers only the answer it has is a
/// picker that reads like there is no question.
const TYPES: { value: NetworkType; label: string; hint: string; ready: boolean }[] = [
  {
    value: "layer2",
    label: "Layer 2",
    hint: "A bridge on each member's uplink. Give it a VLAN to make it an access network onto that one VLAN.",
    ready: true,
  },
  {
    value: "layer3",
    label: "Layer 3",
    hint: "Routed. Not built by this appliance yet.",
    ready: false,
  },
  {
    value: "vxlan",
    label: "VXLAN",
    hint: "Overlay. Not built by this appliance yet.",
    ready: false,
  },
];

const BOND_MODES: { value: BondMode; label: string; hint: string }[] = [
  {
    value: "802.3ad",
    label: "LACP (802.3ad)",
    hint: "Both ports carry traffic. The switch must have a matching port channel.",
  },
  {
    value: "active-backup",
    label: "Active/backup",
    hint: "One port carries traffic, the other waits. Needs nothing from the switch.",
  },
  {
    value: "balance-xor",
    label: "Balance XOR",
    hint: "Both ports carry traffic, hashed. Needs static aggregation on the switch.",
  },
];

/// Define an External network across a cluster, or change one.
///
/// One dialog for both, because the fields an External network has do not
/// change because it already exists — and two forms would be two places for
/// the every-member rule to be spelled differently.
///
/// That rule is what the shape follows: an External network is on every
/// member or on none. A machine that fails over onto a node where its network
/// does not resolve is the failure HA exists to prevent, so this collects at
/// least one port per member and will not submit until every member has one —
/// rather than accepting a partial definition and leaving the operator to
/// discover the hole during an outage.
///
/// Ports are per member and plural because both are facts about cabling. The
/// same network can arrive on `nic1` here and `nic3` there; a member with two
/// spare ports can have redundancy where a member with one cannot. Two or more
/// on a member is a bond, built for the operator rather than asked of them on
/// each node's Interfaces page first.
export function CreateExternalNetworkDialog({
  clusters,
  inventory,
  editing,
  onClose,
  onCreated,
}: {
  clusters: ClusterView[];
  /// Every member's links, for the per-node uplink pickers. Null when the
  /// environment read failed, which is the one case the pickers fall back to
  /// free text.
  inventory: InventoryResponse | null;
  /// The definition being changed, or absent to define a new one.
  editing?: ExternalNetworkEdit;
  onClose: () => void;
  onCreated: (message: string) => void;
}) {
  const [cluster, setCluster] = useState(editing?.cluster ?? clusters[0]?.name ?? "");
  const [name, setName] = useState(editing?.network.name ?? "");
  const [bridge, setBridge] = useState(editing?.network.bridge ?? "");
  const [networkType, setNetworkType] = useState<NetworkType>(editing?.network.type ?? "layer2");
  const [vlan, setVlan] = useState(
    editing?.network.vlan === undefined ? "" : String(editing.network.vlan),
  );
  const [bondMode, setBondMode] = useState<BondMode>(editing?.network.bond ?? "802.3ad");
  const [uplinks, setUplinks] = useState<Record<string, string[]>>(() =>
    Object.fromEntries(
      (editing?.network.uplinks ?? []).map((uplink) => [uplink.node, uplink.interfaces]),
    ),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selected = clusters.find((candidate) => candidate.name === cluster) ?? null;
  const members = useMemo(() => selected?.nodes.map((node) => node.node) ?? [], [selected]);

  /// What each member could carry this network on. Physical adapters and
  /// bonds only: a bridge is what this network *builds*, and offering an
  /// existing one as its uplink would nest a bridge inside a bridge. A bond
  /// the operator already built is offered as one port, which is how a bond
  /// with settings this dialog does not expose still gets used.
  const candidatesFor = (node: string): string[] =>
    linksByMember(inventory)
      .filter(
        (row) =>
          row.node === node &&
          (row.link.kind === "ethernet" || row.link.kind === "bond") &&
          !row.link.management &&
          row.link.controller === null,
      )
      .map((row) => row.link.name);

  const portsOf = (node: string): string[] => uplinks[node] ?? [];

  const toggle = (node: string, port: string) =>
    setUplinks((current) => {
      const ports = current[node] ?? [];
      return {
        ...current,
        // Kept in the order they were picked: the first is what active/backup
        // rests on, so the operator's first click is a decision.
        [node]: ports.includes(port) ? ports.filter((p) => p !== port) : [...ports, port],
      };
    });

  /// Free text is the fallback when a member could not be asked what it has.
  /// A list rather than one name, so a bond can still be described.
  const setFreeText = (node: string, text: string) =>
    setUplinks((current) => ({
      ...current,
      [node]: text
        .split(/[,\s]+/)
        .map((part) => part.trim())
        .filter(Boolean),
    }));

  const missing = members.filter((node) => portsOf(node).length === 0);
  const tagged = vlan.trim() !== "";
  const badVlan = tagged && (!/^\d+$/.test(vlan.trim()) || Number(vlan) < 1 || Number(vlan) > 4094);
  const vlanId = tagged && !badVlan ? Number(vlan) : undefined;

  const bonding = members.some((node) => portsOf(node).length > 1);
  const bondName = bondNameFor(name.trim());
  const bondFits = !bonding || bondNameFits(bondName, vlanId);

  const ready =
    cluster !== "" &&
    name.trim() !== "" &&
    bridge.trim() !== "" &&
    networkType === "layer2" &&
    members.length > 0 &&
    missing.length === 0 &&
    !badVlan &&
    bondFits;

  const submit = async () => {
    setBusy(true);
    setError(null);
    const seats: Uplink[] = members.map((node) => ({ node, interfaces: portsOf(node) }));
    const body: ExternalNetworkCreate = {
      name: name.trim(),
      bridge: bridge.trim(),
      type: networkType,
      uplinks: seats,
      ...(vlanId === undefined ? {} : { vlan: vlanId }),
      // Only when some member is bonded: an unbonded network saying how it
      // would bond is a setting that does nothing, recorded forever.
      ...(bonding ? { bond: bondMode } : {}),
    };
    try {
      if (editing) {
        await updateExternalNetwork(editing.cluster, editing.network.name, body);
        onCreated(`${editing.network.name} rebuilt on every member of ${editing.cluster}.`);
      } else {
        await createExternalNetwork(cluster, body);
        onCreated(`${name.trim()} defined on every member of ${cluster}.`);
      }
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) return;
      setError(
        err instanceof Error
          ? err.message
          : editing
            ? "The network could not be changed."
            : "The network could not be created.",
      );
    } finally {
      setBusy(false);
    }
  };

  const renamedBridge = editing !== undefined && bridge.trim() !== editing.network.bridge;

  return (
    <ModalShell onClose={busy ? () => {} : onClose}>
      <ModalHeader
        title={editing ? `Edit ${editing.network.name}` : "Create External network"}
        subtitle={
          editing
            ? "The change is built on every member before the cluster records it."
            : "Virtual machine traffic, on an identically named bridge on every member."
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

        {clusters.length > 1 && !editing && (
          <Field label="Cluster" htmlFor="external-cluster">
            <SelectInput
              id="external-cluster"
              value={cluster}
              onChange={(next) => {
                setCluster(next);
                // The uplinks named members of the old cluster; keeping them
                // would submit interfaces for nodes that are not in this one.
                setUplinks({});
              }}
              mono
            >
              {clusters.map((candidate) => (
                <option key={candidate.name} value={candidate.name}>
                  {candidate.name}
                </option>
              ))}
            </SelectInput>
          </Field>
        )}

        <Field
          label="Name"
          htmlFor="external-name"
          hint={
            editing
              ? "The name is what a machine's adapter refers to, so it cannot change — renaming it would strand every machine on this network."
              : "What machines attach to, and what every member calls it."
          }
          required
        >
          <TextInput
            id="external-name"
            value={name}
            onChange={setName}
            mono
            autoFocus={!editing}
            disabled={editing !== undefined}
          />
        </Field>

        <Field
          label="Network type"
          htmlFor="external-type"
          hint={TYPES.find((type) => type.value === networkType)?.hint}
        >
          <SelectInput
            id="external-type"
            value={networkType}
            onChange={(next) => setNetworkType(next as NetworkType)}
          >
            {TYPES.map((type) => (
              <option key={type.value} value={type.value} disabled={!type.ready}>
                {type.label}
                {type.ready ? "" : " — not yet"}
              </option>
            ))}
          </SelectInput>
        </Field>

        <Field
          label="Bridge"
          htmlFor="external-bridge"
          hint="The bridge built on each member. Identical everywhere — a machine's network must resolve the same on whichever node it lands."
          required
        >
          <TextInput
            id="external-bridge"
            value={bridge}
            onChange={setBridge}
            placeholder="vmbr1"
            mono
          />
        </Field>

        <Field
          label="VLAN ID"
          htmlFor="external-vlan"
          hint="1–4094. Leave it empty to pass every tag through instead, and let each machine carry its own."
          error={badVlan ? "A VLAN ID is a number from 1 to 4094." : undefined}
        >
          <TextInput
            id="external-vlan"
            value={vlan}
            onChange={setVlan}
            placeholder="Untagged — machines carry their own tags"
            inputMode="numeric"
            mono
            invalid={badVlan}
          />
        </Field>

        {/* The ports on each member, listed rather than defaulted: which
            physical port carries VM traffic is a cabling fact this console
            cannot guess, and guessing it wrong is a machine with no network. */}
        <div className="flex flex-col gap-3">
          <div className="text-[13px] font-semibold text-[var(--qz-fg-2)]">
            Uplinks on each member
          </div>
          <div className="text-[12px] text-[var(--qz-fg-4)]">
            One port per member, or two and more to bond them. Bonding is per member because
            cabling is — a node with one spare port still gets the network.
          </div>
          {members.length === 0 ? (
            <div className="text-[13px] text-[var(--qz-fg-4)]">
              This cluster has no members to build on.
            </div>
          ) : (
            members.map((node) => {
              const options = candidatesFor(node);
              const ports = portsOf(node);
              return (
                <Field
                  key={node}
                  label={shortNodeName(node)}
                  htmlFor={`uplink-${node}`}
                  hint={ports.length > 1 ? `Bonded as ${bondName} on this member.` : undefined}
                  required
                >
                  {options.length > 0 ? (
                    <CheckList>
                      {options.map((option) => (
                        <CheckRow
                          key={option}
                          checked={ports.includes(option)}
                          onChange={() => toggle(node, option)}
                        >
                          <span className="qz-mono text-[13px] text-[var(--qz-fg-2)]">
                            {option}
                          </span>
                        </CheckRow>
                      ))}
                    </CheckList>
                  ) : (
                    // The member could not be asked what it has. Free text
                    // rather than an empty picker: the operator knows the
                    // node's cabling even when this console cannot read it.
                    <TextInput
                      id={`uplink-${node}`}
                      value={ports.join(", ")}
                      onChange={(next) => setFreeText(node, next)}
                      placeholder="nic1, nic2"
                      mono
                      invalid={ports.length === 0}
                    />
                  )}
                </Field>
              );
            })
          )}
        </div>

        {/* Only when it means something. A bonding mode on a network where no
            member has two ports is a setting that does nothing, and the
            record would keep it forever. */}
        {bonding && (
          <Field
            label="Bond mode"
            htmlFor="external-bond"
            hint={BOND_MODES.find((mode) => mode.value === bondMode)?.hint}
          >
            <SelectInput
              id="external-bond"
              value={bondMode}
              onChange={(next) => setBondMode(next as BondMode)}
            >
              {BOND_MODES.map((mode) => (
                <option key={mode.value} value={mode.value}>
                  {mode.label}
                </option>
              ))}
            </SelectInput>
          </Field>
        )}

        {bonding && !bondFits && (
          <div className="callout callout-crit">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-danger)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              Bonding would build <span className="qz-mono">{bondName}</span> on each member, and{" "}
              <span className="qz-mono">
                {vlanId === undefined ? bondName : `${bondName}.${vlanId}`}
              </span>{" "}
              is longer than the 15 characters the kernel allows a link name. Give the network a
              shorter name, or build the bond on Interfaces first and pick it as the uplink.
            </div>
          </div>
        )}

        {missing.length > 0 && members.length > 0 && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              {shortNodeNames(missing)} {missing.length === 1 ? "has" : "have"} no uplink yet. An
              External network is defined on every member or on none — a machine that fails over
              onto a member without it comes up with no network.
            </div>
          </div>
        )}

        {/* The old bridge is not torn down, and saying so beats an operator
            finding a stray link on Interfaces and wondering what left it. */}
        {renamedBridge && (
          <div className="callout callout-warn">
            <AlertTriangle size={17} className="flex-shrink-0 text-[var(--qz-warn)] mt-[1px]" />
            <div className="text-[13px] text-[var(--qz-fg-2)]">
              <span className="qz-mono">{editing?.network.bridge}</span> stays on every member —
              machines may still be attached to it, and this dialog will not pull a network out
              from under a running guest. Move them onto{" "}
              <span className="qz-mono">{bridge.trim() || "the new bridge"}</span>, then remove the
              old link per node on Interfaces.
            </div>
          </div>
        )}

        <ModalFooter
          onCancel={onClose}
          saving={busy}
          disabled={!ready}
          submitLabel={editing ? "Save changes" : "Create network"}
          savingLabel={editing ? "Rebuilding…" : "Creating…"}
          onSubmit={() => void submit()}
        />
      </div>
    </ModalShell>
  );
}

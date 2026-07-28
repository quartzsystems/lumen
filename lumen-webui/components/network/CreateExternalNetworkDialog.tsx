"use client";

import { useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { ModalHeader, ModalShell } from "@/components/ui/Modal";
import { Field, ModalFooter, SelectInput, TextInput } from "@/components/ui/formkit";
import { ApiError } from "@/lib/authClient";
import { shortNodeName, shortNodeNames } from "@/lib/nodeNames";
import {
  createExternalNetwork,
  updateExternalNetwork,
  type ClusterView,
  type ExternalNetwork,
  type ExternalNetworkCreate,
  type Uplink,
} from "@/lib/clusterClient";
import { linksByMember, type InventoryResponse } from "@/lib/inventoryClient";

/// An External network being changed, with the cluster it belongs to. The
/// table this is opened from spans clusters, so a name alone does not
/// identify one.
export interface ExternalNetworkEdit {
  cluster: string;
  network: ExternalNetwork;
}

/// Define an External network across a cluster, or change one.
///
/// One dialog for both, because the fields an External network has do not
/// change because it already exists — and two forms would be two places for
/// the every-member rule to be spelled differently.
///
/// That rule is what the shape follows: an External network is on every
/// member or on none. A machine that fails over onto a node where its network
/// does not resolve is the failure HA exists to prevent, so this collects an
/// uplink per member and will not submit until every member has one — rather
/// than accepting a partial definition and leaving the operator to discover
/// the hole during an outage.
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
  const [mode, setMode] = useState<"trunk" | "access">(editing?.network.mode ?? "trunk");
  const [vlan, setVlan] = useState(
    editing?.network.mode === "access" ? String(editing.network.vlan) : "",
  );
  const [allowed, setAllowed] = useState(
    editing?.network.mode === "trunk" ? editing.network.allowed.join(", ") : "",
  );
  const [uplinks, setUplinks] = useState<Record<string, string>>(() =>
    Object.fromEntries(
      (editing?.network.uplinks ?? []).map((uplink) => [uplink.node, uplink.interface]),
    ),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const selected = clusters.find((candidate) => candidate.name === cluster) ?? null;
  const members = useMemo(
    () => selected?.nodes.map((node) => node.node) ?? [],
    [selected],
  );

  /// What each member could carry this network on. Physical adapters and
  /// bonds only: a bridge is what this network *builds*, and offering an
  /// existing one as its uplink would nest a bridge inside a bridge.
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

  const vlanIds = allowed
    .split(/[,\s]+/)
    .map((part) => part.trim())
    .filter(Boolean);
  const vlanNumbers = vlanIds.map(Number);

  const missing = members.filter((node) => !uplinks[node]);
  const badVlan =
    mode === "access"
      ? !/^\d+$/.test(vlan) || Number(vlan) < 1 || Number(vlan) > 4094
      : vlanNumbers.some((id) => !Number.isInteger(id) || id < 1 || id > 4094);

  const ready =
    cluster !== "" &&
    name.trim() !== "" &&
    bridge.trim() !== "" &&
    members.length > 0 &&
    missing.length === 0 &&
    !badVlan;

  const submit = async () => {
    setBusy(true);
    setError(null);
    const seats: Uplink[] = members.map((node) => ({
      node,
      interface: uplinks[node],
    }));
    const body: ExternalNetworkCreate =
      mode === "access"
        ? { name: name.trim(), bridge: bridge.trim(), uplinks: seats, mode, vlan: Number(vlan) }
        : {
            name: name.trim(),
            bridge: bridge.trim(),
            uplinks: seats,
            mode,
            allowed: vlanNumbers,
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

        <Field label="VLAN" htmlFor="external-mode">
          <SelectInput
            id="external-mode"
            value={mode}
            onChange={(next) => setMode(next as "trunk" | "access")}
          >
            <option value="trunk">Trunk — machines carry their own tags</option>
            <option value="access">Access — one VLAN, untagged to the machine</option>
          </SelectInput>
        </Field>

        {mode === "access" ? (
          <Field
            label="VLAN ID"
            htmlFor="external-vlan"
            hint="1–4094."
            error={vlan !== "" && badVlan ? "A VLAN ID is a number from 1 to 4094." : undefined}
            required
          >
            <TextInput
              id="external-vlan"
              value={vlan}
              onChange={setVlan}
              inputMode="numeric"
              mono
              invalid={vlan !== "" && badVlan}
            />
          </Field>
        ) : (
          <Field
            label="Allowed VLANs"
            htmlFor="external-allowed"
            hint="Leave empty to pass every tag. Otherwise a list, e.g. 10, 20, 30."
            error={allowed !== "" && badVlan ? "Each VLAN ID is a number from 1 to 4094." : undefined}
          >
            <TextInput
              id="external-allowed"
              value={allowed}
              onChange={setAllowed}
              placeholder="10, 20, 30"
              mono
              invalid={allowed !== "" && badVlan}
            />
          </Field>
        )}

        {/* One uplink per member, listed rather than defaulted: which physical
            port carries VM traffic is a cabling fact this console cannot
            guess, and guessing it wrong is a machine with no network. */}
        <div className="flex flex-col gap-3">
          <div className="text-[13px] font-semibold text-[var(--qz-fg-2)]">
            Uplink on each member
          </div>
          {members.length === 0 ? (
            <div className="text-[13px] text-[var(--qz-fg-4)]">
              This cluster has no members to build on.
            </div>
          ) : (
            members.map((node) => {
              const options = candidatesFor(node);
              return (
                <Field
                  key={node}
                  label={shortNodeName(node)}
                  htmlFor={`uplink-${node}`}
                  required
                >
                  {options.length > 0 ? (
                    <SelectInput
                      id={`uplink-${node}`}
                      value={uplinks[node] ?? ""}
                      onChange={(next) => setUplinks({ ...uplinks, [node]: next })}
                      mono
                      invalid={!uplinks[node]}
                    >
                      <option value="">Choose an interface…</option>
                      {options.map((option) => (
                        <option key={option} value={option}>
                          {option}
                        </option>
                      ))}
                    </SelectInput>
                  ) : (
                    // The member could not be asked what it has. Free text
                    // rather than an empty picker: the operator knows the
                    // node's cabling even when this console cannot read it.
                    <TextInput
                      id={`uplink-${node}`}
                      value={uplinks[node] ?? ""}
                      onChange={(next) => setUplinks({ ...uplinks, [node]: next })}
                      placeholder="nic1"
                      mono
                      invalid={!uplinks[node]}
                    />
                  )}
                </Field>
              );
            })
          )}
        </div>

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
